use std::ffi::CString;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use pyo3::ffi::c_str;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyTuple};

use crate::model::{Order, TickSnapshot, Trade};

const PY_HELPER: &str = r#"
import importlib.util
import io
import sys
from contextlib import redirect_stdout

_DM = None
_Listing = None
_Order = None
_OrderDepth = None
_Trade = None
_ConversionObservation = None
_Observation = None
_TradingState = None


def ensure_sys_path(root):
    if root not in sys.path:
        sys.path.insert(0, root)


def bind_datamodel(datamodel_module):
    global _DM, _Listing, _Order, _OrderDepth, _Trade, _ConversionObservation, _Observation, _TradingState
    _DM = datamodel_module
    _Listing = datamodel_module.Listing
    _Order = datamodel_module.Order
    _OrderDepth = datamodel_module.OrderDepth
    _Trade = datamodel_module.Trade
    _ConversionObservation = datamodel_module.ConversionObservation
    _Observation = datamodel_module.Observation
    _TradingState = datamodel_module.TradingState


def load_trader_instance(module_name, trader_file, datamodel_module):
    spec = importlib.util.spec_from_file_location(module_name, trader_file)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Cannot load trader file: {trader_file}")

    module = importlib.util.module_from_spec(spec)
    old_datamodel = sys.modules.get("datamodel")
    old_module = sys.modules.get(module_name)
    sys.modules["datamodel"] = datamodel_module
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    finally:
        if old_datamodel is None:
            sys.modules.pop("datamodel", None)
        else:
            sys.modules["datamodel"] = old_datamodel
        if old_module is None:
            sys.modules.pop(module_name, None)
        else:
            sys.modules[module_name] = old_module

    if not hasattr(module, "Trader"):
        raise RuntimeError("Trader file does not define a Trader class")

    return module.Trader()


def _build_order_depths(payload):
    order_depths = {}
    for symbol, rows in payload.items():
        buy_rows, sell_rows = rows
        depth = _OrderDepth()
        depth.buy_orders = {int(price): int(volume) for price, volume in buy_rows}
        depth.sell_orders = {int(price): int(volume) for price, volume in sell_rows}
        order_depths[str(symbol)] = depth
    return order_depths


def _build_trade_dict(payload):
    out = {}
    for symbol, rows in payload.items():
        out[str(symbol)] = [
            _Trade(str(trade_symbol), int(price), int(quantity), str(buyer), str(seller), int(timestamp))
            for trade_symbol, price, quantity, buyer, seller, timestamp in rows
        ]
    return out


def _build_conversion_observations(payload):
    return {
        str(product): _ConversionObservation(
            float(values[0]),
            float(values[1]),
            float(values[2]),
            float(values[3]),
            float(values[4]),
            float(values[5]),
            float(values[6]),
        )
        for product, values in payload.items()
    }


def _build_state(
    trader_data,
    timestamp,
    listing_symbols,
    order_depth_payload,
    own_trade_payload,
    market_trade_payload,
    position_payload,
    plain_obs_payload,
    conversion_payload,
):
    listings = {str(product): _Listing(str(product), str(product), "SEASHELLS") for product in listing_symbols}
    order_depths = _build_order_depths(order_depth_payload)
    own_trades = _build_trade_dict(own_trade_payload)
    market_trades = _build_trade_dict(market_trade_payload)
    position = {str(product): int(qty) for product, qty in position_payload}
    plain = {str(product): int(value) for product, value in plain_obs_payload}
    observations = _Observation(
        plainValueObservations=plain,
        conversionObservations=_build_conversion_observations(conversion_payload),
    )
    return _TradingState(
        str(trader_data),
        int(timestamp),
        listings,
        order_depths,
        own_trades,
        market_trades,
        position,
        observations,
    )


def _normalize_orders(raw_orders):
    if not isinstance(raw_orders, dict):
        raise RuntimeError(f"Trader.run returned non-dict orders: {type(raw_orders)}")

    normalized = {}
    for symbol, orders in raw_orders.items():
        if orders is None:
            continue
        if not isinstance(symbol, str):
            raise RuntimeError("Orders dictionary keys must be strings")
        if not isinstance(orders, list):
            raise RuntimeError(f"Orders for {symbol} are not a list")

        normalized_orders = []
        for order in orders:
            if isinstance(order, _Order):
                normalized_orders.append((str(order.symbol), int(order.price), int(order.quantity)))
                continue
            if hasattr(order, "symbol") and hasattr(order, "price") and hasattr(order, "quantity"):
                normalized_orders.append((str(order.symbol), int(order.price), int(order.quantity)))
                continue
            if isinstance(order, (tuple, list)) and len(order) == 3:
                normalized_orders.append((str(order[0]), int(order[1]), int(order[2])))
                continue
            raise RuntimeError(f"Unrecognized order type in {symbol}: {order}")

        normalized[symbol] = normalized_orders

    return normalized


def _normalize_run_output(output):
    if isinstance(output, tuple):
        if len(output) == 3:
            orders, conversions, trader_data = output
        elif len(output) == 2:
            orders, trader_data = output
            conversions = 0
        elif len(output) == 1:
            orders = output[0]
            conversions = 0
            trader_data = ""
        else:
            raise RuntimeError("Trader.run returned tuple with unsupported length")
    else:
        orders = output
        conversions = 0
        trader_data = ""

    return _normalize_orders(orders), int(conversions), str(trader_data)


def invoke_trader_on_payload(
    trader,
    trader_data,
    timestamp,
    listing_symbols,
    order_depth_payload,
    own_trade_payload,
    market_trade_payload,
    position_payload,
    plain_obs_payload,
    conversion_payload,
):
    state = _build_state(
        trader_data,
        timestamp,
        listing_symbols,
        order_depth_payload,
        own_trade_payload,
        market_trade_payload,
        position_payload,
        plain_obs_payload,
        conversion_payload,
    )
    captured_stdout = io.StringIO()
    with redirect_stdout(captured_stdout):
        output = trader.run(state)
    orders, conversions, next_trader_data = _normalize_run_output(output)
    return orders, conversions, next_trader_data, captured_stdout.getvalue()
"#;

const DATAMODEL_SOURCE: &str = r#"
from __future__ import annotations

import json
from json import JSONEncoder
from typing import Dict, List, Optional

Time = int
Symbol = str
Product = str
Position = int
UserId = str
ObservationValue = int


class Listing:
    def __init__(self, symbol: Symbol, product: Product, denomination: Product):
        self.symbol = symbol
        self.product = product
        self.denomination = denomination


class ConversionObservation:
    def __init__(
        self,
        bidPrice: float,
        askPrice: float,
        transportFees: float,
        exportTariff: float,
        importTariff: float,
        sugarPrice: float,
        sunlightIndex: float,
    ):
        self.bidPrice = bidPrice
        self.askPrice = askPrice
        self.transportFees = transportFees
        self.exportTariff = exportTariff
        self.importTariff = importTariff
        self.sugarPrice = sugarPrice
        self.sunlightIndex = sunlightIndex


class Observation:
    def __init__(
        self,
        plainValueObservations: Optional[Dict[Product, ObservationValue]] = None,
        conversionObservations: Optional[Dict[Product, ConversionObservation]] = None,
    ) -> None:
        self.plainValueObservations = plainValueObservations or {}
        self.conversionObservations = conversionObservations or {}


class Order:
    def __init__(self, symbol: Symbol, price: int, quantity: int) -> None:
        self.symbol = symbol
        self.price = price
        self.quantity = quantity

    def __repr__(self) -> str:
        return f"Order({self.symbol}, {self.price}, {self.quantity})"


class OrderDepth:
    def __init__(self):
        self.buy_orders: Dict[int, int] = {}
        self.sell_orders: Dict[int, int] = {}


class Trade:
    def __init__(
        self,
        symbol: Symbol,
        price: int,
        quantity: int,
        buyer: Optional[UserId] = None,
        seller: Optional[UserId] = None,
        timestamp: int = 0,
    ) -> None:
        self.symbol = symbol
        self.price = price
        self.quantity = quantity
        self.buyer = buyer
        self.seller = seller
        self.timestamp = timestamp

    def __repr__(self) -> str:
        return (
            f"Trade({self.symbol}, {self.price}, {self.quantity}, "
            f"{self.buyer}, {self.seller}, {self.timestamp})"
        )


class TradingState:
    def __init__(
        self,
        traderData: str,
        timestamp: Time,
        listings: Dict[Symbol, Listing],
        order_depths: Dict[Symbol, OrderDepth],
        own_trades: Dict[Symbol, List[Trade]],
        market_trades: Dict[Symbol, List[Trade]],
        position: Dict[Product, Position],
        observations: Observation,
    ):
        self.traderData = traderData
        self.timestamp = timestamp
        self.listings = listings
        self.order_depths = order_depths
        self.own_trades = own_trades
        self.market_trades = market_trades
        self.position = position
        self.observations = observations

    def toJSON(self) -> str:
        return json.dumps(self, default=lambda o: o.__dict__, sort_keys=True)


class ProsperityEncoder(JSONEncoder):
    def default(self, o):
        return o.__dict__
"#;

pub struct TraderInvocation<'a> {
    pub trader_data: &'a str,
    pub tick: &'a TickSnapshot,
    pub own_trades_prev: &'a IndexMap<String, Vec<Trade>>,
    pub market_trades_prev: &'a IndexMap<String, Vec<Trade>>,
    pub position: &'a IndexMap<String, i64>,
}

pub struct TraderRunResult {
    pub orders_by_symbol: IndexMap<String, Vec<Order>>,
    pub conversions: i64,
    pub trader_data: String,
    pub stdout: String,
}

pub struct PythonTrader {
    trader: Py<PyAny>,
    invoke_fn: Py<PyAny>,
}

fn python_import_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl PythonTrader {
    pub fn new(workspace_root: &Path, trader_file: &Path) -> Result<Self> {
        // Trader module loading temporarily rewires sys.modules["datamodel"] so
        // parallel imports must not overlap.
        let _import_guard = python_import_lock()
            .lock()
            .expect("python import lock should not be poisoned");
        Python::attach(|py| -> Result<Self> {
            let helper_code = CString::new(PY_HELPER)?;
            let helper = PyModule::from_code(
                py,
                helper_code.as_c_str(),
                c_str!("rust_backtester_helper.py"),
                c_str!("rust_backtester_helper"),
            )?;

            let workspace_root_str = workspace_root.to_string_lossy().to_string();
            helper
                .getattr("ensure_sys_path")?
                .call1((workspace_root_str,))?;

            let trader_dir = trader_file
                .parent()
                .unwrap_or(workspace_root)
                .to_string_lossy()
                .to_string();
            helper.getattr("ensure_sys_path")?.call1((trader_dir,))?;

            let datamodel_module = embedded_datamodel(py)?;
            helper
                .getattr("bind_datamodel")?
                .call1((datamodel_module.clone(),))?;

            let module_name = format!(
                "user_trader_{}",
                safe_filename(
                    trader_file
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("trader")
                )
            );
            let trader_path = trader_file.to_string_lossy().to_string();
            let trader = helper.getattr("load_trader_instance")?.call1((
                module_name,
                trader_path,
                datamodel_module.clone(),
            ))?;
            let invoke_fn = helper.getattr("invoke_trader_on_payload")?;

            Ok(Self {
                trader: trader.unbind(),
                invoke_fn: invoke_fn.unbind(),
            })
        })
    }

    pub fn run_tick(&mut self, input: &TraderInvocation<'_>) -> Result<TraderRunResult> {
        Python::attach(|py| -> Result<TraderRunResult> {
            let listing_symbols = PyList::new(py, input.tick.products.keys())?;
            let order_depth_payload = build_order_depth_payload(py, input.tick)?;
            let own_trade_payload = build_trade_payload(py, input.own_trades_prev)?;
            let market_trade_payload = build_trade_payload(py, input.market_trades_prev)?;
            let position_payload = PyList::new(py, input.position.iter())?;
            let plain_obs_payload = PyList::new(py, input.tick.observations.plain.iter())?;
            let conversion_payload = build_conversion_payload(py, input.tick)?;

            let output_and_stdout = self.invoke_fn.bind(py).call1((
                self.trader.bind(py),
                input.trader_data,
                input.tick.timestamp,
                listing_symbols,
                order_depth_payload,
                own_trade_payload,
                market_trade_payload,
                position_payload,
                plain_obs_payload,
                conversion_payload,
            ))?;

            let tuple = output_and_stdout.cast::<PyTuple>().map_err(|_| {
                anyhow::anyhow!("invoke_trader_on_payload returned non-tuple payload")
            })?;
            let orders_obj = tuple.get_item(0)?;
            let conversions = tuple.get_item(1)?.extract::<i64>()?;
            let trader_data = tuple
                .get_item(2)?
                .str()?
                .to_str()
                .context("traderData could not be converted to utf-8")?
                .to_string();
            let stdout = tuple
                .get_item(3)?
                .str()?
                .to_str()
                .context("stdout could not be converted to utf-8")?
                .to_string();

            Ok(TraderRunResult {
                orders_by_symbol: extract_orders(orders_obj)?,
                conversions,
                trader_data,
                stdout,
            })
        })
    }
}

fn embedded_datamodel<'py>(py: Python<'py>) -> Result<Bound<'py, PyModule>> {
    let datamodel_code = CString::new(DATAMODEL_SOURCE)?;
    let datamodel_module = PyModule::from_code(
        py,
        datamodel_code.as_c_str(),
        c_str!("datamodel.py"),
        c_str!("engine.datamodel"),
    )?;
    let engine_module = PyModule::new(py, "engine")?;
    engine_module.setattr("datamodel", datamodel_module.clone())?;

    let sys = py.import("sys")?;
    let modules = sys.getattr("modules")?;
    modules.set_item("engine", engine_module)?;
    modules.set_item("engine.datamodel", datamodel_module.clone())?;

    Ok(datamodel_module)
}

fn build_order_depth_payload<'py>(
    py: Python<'py>,
    tick: &TickSnapshot,
) -> Result<Bound<'py, PyDict>> {
    let payload = PyDict::new(py);
    for (product, snapshot) in &tick.products {
        let buy_levels = PyList::new(
            py,
            snapshot
                .bids
                .iter()
                .map(|level| (level.price, level.volume)),
        )?;
        let sell_levels = PyList::new(
            py,
            snapshot
                .asks
                .iter()
                .map(|level| (level.price, -level.volume)),
        )?;
        payload.set_item(product, (buy_levels, sell_levels))?;
    }
    Ok(payload)
}

fn build_trade_payload<'py>(
    py: Python<'py>,
    trades: &IndexMap<String, Vec<Trade>>,
) -> Result<Bound<'py, PyDict>> {
    let payload = PyDict::new(py);
    for (symbol, rows) in trades {
        let list = PyList::new(
            py,
            rows.iter().map(|trade| {
                (
                    trade.symbol.clone(),
                    trade.price,
                    trade.quantity,
                    trade.buyer.clone(),
                    trade.seller.clone(),
                    trade.timestamp,
                )
            }),
        )?;
        payload.set_item(symbol, list)?;
    }
    Ok(payload)
}

fn build_conversion_payload<'py>(
    py: Python<'py>,
    tick: &TickSnapshot,
) -> Result<Bound<'py, PyDict>> {
    let payload = PyDict::new(py);
    for (product, values) in &tick.observations.conversion {
        payload.set_item(
            product,
            (
                values.get("bidPrice").copied().unwrap_or(0.0),
                values.get("askPrice").copied().unwrap_or(0.0),
                values.get("transportFees").copied().unwrap_or(0.0),
                values.get("exportTariff").copied().unwrap_or(0.0),
                values.get("importTariff").copied().unwrap_or(0.0),
                values.get("sugarPrice").copied().unwrap_or(0.0),
                values.get("sunlightIndex").copied().unwrap_or(0.0),
            ),
        )?;
    }
    Ok(payload)
}

fn extract_orders(raw_orders: Bound<'_, PyAny>) -> Result<IndexMap<String, Vec<Order>>> {
    let orders_dict = raw_orders
        .cast::<PyDict>()
        .map_err(|_| anyhow::anyhow!("normalized orders payload is not a dict"))?;
    let mut normalized = IndexMap::new();
    for (symbol_obj, rows_obj) in orders_dict.iter() {
        let symbol = symbol_obj.extract::<String>()?;
        let rows = rows_obj
            .cast::<PyList>()
            .map_err(|_| anyhow::anyhow!("normalized orders for {symbol} are not a list"))?;
        let mut product_orders = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let tuple = row
                .cast::<PyTuple>()
                .map_err(|_| anyhow::anyhow!("normalized order row for {symbol} is not a tuple"))?;
            if tuple.len() != 3 {
                bail!("normalized order row for {symbol} has unexpected length");
            }
            product_orders.push(Order {
                symbol: tuple.get_item(0)?.extract::<String>()?,
                price: tuple.get_item(1)?.extract::<i64>()?,
                quantity: tuple.get_item(2)?.extract::<i64>()?,
            });
        }
        normalized.insert(symbol, product_orders);
    }
    Ok(normalized)
}

fn safe_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}
