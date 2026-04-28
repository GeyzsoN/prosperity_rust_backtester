use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde_json::Value;
use time::{OffsetDateTime, format_description};

use crate::jsonfmt::{
    index_object, json_f64, json_i64, json_usize, object, pretty_json_bytes, python_float_string,
    sorted_json_bytes,
};
use crate::model::{
    ArtifactSet, MatchingConfig, MetadataOverrides, NormalizedDataset, Order, RunMetrics,
    RunOutput, RunRequest, TickSnapshot, Trade, load_dataset,
};
use crate::pytrader::{PythonTrader, TraderInvocation};

const DEFAULT_POSITION_LIMIT: i64 = 100;
const LOG_CHAR_LIMIT: usize = 3750;
const ACTIVITY_HEADER: &str = "day;timestamp;product;bid_price_1;bid_volume_1;bid_price_2;bid_volume_2;bid_price_3;bid_volume_3;ask_price_1;ask_volume_1;ask_price_2;ask_volume_2;ask_price_3;ask_volume_3;mid_price;profit_and_loss";

#[derive(Clone, Copy)]
struct BookLevel {
    price: i64,
    volume: i64,
}

#[derive(Clone)]
struct FlatOrderRow {
    symbol: String,
    price: i64,
    quantity: i64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MarketReplayMode {
    RawCsvTape,
    ExecutedTradeHistory,
}

#[derive(Clone, Copy)]
struct RestingSubmissionOrder {
    price: i64,
    quantity: i64,
}

pub fn default_output_root() -> PathBuf {
    let crate_root = project_root();
    if crate_root.join("Cargo.toml").is_file() {
        crate_root.join("runs")
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("runs")
    }
}

pub fn project_root() -> PathBuf {
    static PROJECT_ROOT: OnceLock<PathBuf> = OnceLock::new();
    PROJECT_ROOT.get_or_init(resolve_project_root).clone()
}

pub fn workspace_root() -> PathBuf {
    let crate_root = project_root();
    if crate_root.join("datasets").is_dir()
        || crate_root.join("traders").is_dir()
        || crate_root.join("opensource").is_dir()
    {
        return crate_root;
    }
    if let Some(parent) = crate_root.parent() {
        if parent.join("datasets").is_dir()
            || parent.join("traders").is_dir()
            || parent.join("opensource").is_dir()
        {
            return parent.to_path_buf();
        }
    }
    crate_root
}

fn resolve_project_root() -> PathBuf {
    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(root) = find_project_root(&current_dir) {
            return root;
        }
    }
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            if let Some(root) = find_project_root(parent) {
                return root;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("src").is_dir() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

pub fn display_path(path: &Path) -> String {
    if path.is_relative() {
        return path_string(path);
    }

    let mut bases = vec![project_root(), workspace_root()];
    if let Ok(current_dir) = std::env::current_dir() {
        bases.push(current_dir);
    }

    for base in bases {
        if let Ok(relative) = path.strip_prefix(&base) {
            if !relative.as_os_str().is_empty() {
                return path_string(relative);
            }
        }
    }

    path_string(path)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn market_replay_mode(dataset: &NormalizedDataset) -> MarketReplayMode {
    if dataset
        .metadata
        .get("source_format")
        .and_then(Value::as_str)
        == Some("imc_csv")
    {
        MarketReplayMode::RawCsvTape
    } else {
        MarketReplayMode::ExecutedTradeHistory
    }
}

pub fn run_backtest(request: &RunRequest) -> Result<RunOutput> {
    let dataset = if let Some(dataset) = &request.dataset_override {
        dataset.clone()
    } else {
        load_dataset(&request.dataset_file)?
    };
    let replay_mode = market_replay_mode(&dataset);
    let mut ticks: Vec<&TickSnapshot> = dataset
        .ticks
        .iter()
        .filter(|tick| request.day.is_none_or(|day| tick.day == Some(day)))
        .collect();
    ticks.sort_by_key(|tick| (tick.day, tick.timestamp));
    if ticks.is_empty() {
        bail!("No ticks available for selected dataset/day");
    }

    let mut trader = PythonTrader::new(&workspace_root(), &request.trader_file)?;
    let run_id = resolve_run_id(request)?;
    let run_dir = request.output_root.join(&run_id);
    let need_submission_log = request.persist || request.write_submission_log;
    let write_bundle = request.persist || request.write_bundle || request.materialize_artifacts;
    let full_artifacts = request.persist || request.materialize_artifacts;
    if need_submission_log || request.write_metrics || write_bundle {
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    }

    let recorded_trader_path = request
        .metadata_overrides
        .recorded_trader_path
        .clone()
        .unwrap_or_else(|| display_path(&request.trader_file));
    let recorded_dataset_path = request
        .metadata_overrides
        .recorded_dataset_path
        .clone()
        .unwrap_or_else(|| display_path(&request.dataset_file));
    let bundle_generated_at = resolve_generated_at(&request.metadata_overrides)?;
    let metrics_generated_at = resolve_generated_at(&request.metadata_overrides)?;

    let mut cash_by_product: IndexMap<String, f64> = dataset
        .products
        .iter()
        .cloned()
        .map(|product| (product, 0.0))
        .collect();
    let mut position: IndexMap<String, i64> = IndexMap::new();
    let mut own_trades_prev: IndexMap<String, Vec<Trade>> = IndexMap::new();
    let mut market_trades_prev: IndexMap<String, Vec<Trade>> = IndexMap::new();
    let mut trader_data = String::new();
    let mut last_stable_mark_by_product: IndexMap<String, f64> = IndexMap::new();

    let mut own_trade_count = 0usize;
    let mut final_pnl_total = 0.0f64;
    let mut final_pnl_by_product: IndexMap<String, f64> = dataset
        .products
        .iter()
        .cloned()
        .map(|product| (product, 0.0))
        .collect();

    let mut activity_rows: Vec<String> = if need_submission_log {
        vec![ACTIVITY_HEADER.to_string()]
    } else {
        Vec::new()
    };
    let mut own_trade_rows: Vec<Value> = Vec::new();
    let mut combined_trade_history: Vec<Value> = Vec::new();
    let mut sandbox_rows: Vec<Value> = Vec::new();
    let mut timeline: Vec<Value> = Vec::new();
    let mut pnl_series: Vec<Value> = Vec::new();

    let tick_count = ticks.len();
    for tick in ticks {
        let tick_result = trader.run_tick(&TraderInvocation {
            trader_data: &trader_data,
            tick,
            own_trades_prev: &own_trades_prev,
            market_trades_prev: &market_trades_prev,
            position: &position,
        })?;

        trader_data = tick_result.trader_data;
        let (orders_by_symbol, limit_messages) =
            enforce_position_limits(&position, tick_result.orders_by_symbol);

        let mut own_trades_tick: IndexMap<String, Vec<Trade>> = IndexMap::new();
        let mut market_trades_next: IndexMap<String, Vec<Trade>> = IndexMap::new();
        let mut orders_flat: Vec<FlatOrderRow> = Vec::new();

        for product in &dataset.products {
            let snapshot = tick.products.get(product);
            let mut bids = snapshot_to_book(snapshot, true);
            let mut asks = snapshot_to_book(snapshot, false);
            let market_trades = tick
                .market_trades
                .get(product)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let (symbol_own_trades, remaining_market, symbol_orders) = match replay_mode {
                MarketReplayMode::RawCsvTape => match_orders_for_symbol_raw_csv_tape(
                    product,
                    orders_by_symbol.get(product).cloned().unwrap_or_default(),
                    &mut bids,
                    &mut asks,
                    market_trades,
                    &mut position,
                    &mut cash_by_product,
                    tick.timestamp,
                    &request.matching,
                    full_artifacts,
                ),
                MarketReplayMode::ExecutedTradeHistory => match_orders_for_symbol(
                    product,
                    orders_by_symbol.get(product).cloned().unwrap_or_default(),
                    &mut bids,
                    &mut asks,
                    market_trades,
                    &mut position,
                    &mut cash_by_product,
                    tick.timestamp,
                    &request.matching,
                    full_artifacts,
                ),
            };

            if need_submission_log {
                for trade in &remaining_market {
                    combined_trade_history.push(trade_history_json(trade, tick.day));
                }
                for trade in &symbol_own_trades {
                    combined_trade_history.push(trade_history_json(trade, tick.day));
                    if full_artifacts {
                        own_trade_rows.push(trade_history_json(trade, tick.day));
                    }
                }
            }
            if full_artifacts {
                orders_flat.extend(symbol_orders);
            }

            if !symbol_own_trades.is_empty() {
                own_trade_count += symbol_own_trades.len();
                own_trades_tick.insert(product.clone(), symbol_own_trades);
            }
            if !remaining_market.is_empty() {
                market_trades_next.insert(product.clone(), remaining_market);
            }
        }

        let algorithm_logs = truncate_logs(&tick_result.stdout);
        let sandbox_log = limit_messages.join("\n");

        let mut pnl_by_product = IndexMap::with_capacity(dataset.products.len());
        let mut mark_prices = IndexMap::with_capacity(dataset.products.len());
        for product in &dataset.products {
            let snapshot = tick.products.get(product);
            if let Some(stable_mark) = resolve_stable_mark(snapshot) {
                last_stable_mark_by_product.insert(product.clone(), stable_mark);
            }
            let mark_price =
                resolve_mark_price(snapshot, last_stable_mark_by_product.get(product).copied());
            if let Some(price) = mark_price {
                mark_prices.insert(product.clone(), price);
            }
            let mark_to_market = mark_price
                .map(|price| position.get(product).copied().unwrap_or(0) as f64 * price)
                .unwrap_or(0.0);
            let pnl = cash_by_product.get(product).copied().unwrap_or(0.0) + mark_to_market;
            pnl_by_product.insert(product.clone(), pnl);

            if need_submission_log {
                activity_rows.push(format_activity_row(
                    tick, product, snapshot, mark_price, pnl,
                ));
            }
        }

        final_pnl_total = pnl_by_product.values().sum();
        final_pnl_by_product = pnl_by_product.clone();

        if need_submission_log {
            sandbox_rows.push(object(vec![
                ("day", tick.day.map(json_i64).unwrap_or(Value::Null)),
                ("timestamp", json_i64(tick.timestamp)),
                ("sandboxLog", Value::String(sandbox_log.clone())),
                ("lambdaLog", Value::String(algorithm_logs.clone())),
            ]));
        }

        if write_bundle {
            pnl_series.push(object(vec![
                ("timestamp", json_i64(tick.timestamp)),
                ("total", json_f64(final_pnl_total)?),
                (
                    "by_product",
                    index_object(&indexmap_f64_to_json(&pnl_by_product)?),
                ),
            ]));

            if full_artifacts {
                timeline.push(build_timeline_row(
                    tick,
                    &orders_flat,
                    &own_trades_tick,
                    &market_trades_next,
                    &position,
                    &mark_prices,
                    &pnl_by_product,
                    final_pnl_total,
                    &sandbox_log,
                    &algorithm_logs,
                    tick_result.conversions,
                    &trader_data,
                )?);
            }
        }

        own_trades_prev = own_trades_tick;
        market_trades_prev = market_trades_next;
    }

    let metrics = RunMetrics {
        run_id: run_id.clone(),
        dataset_id: dataset.dataset_id.clone(),
        dataset_path: recorded_dataset_path.clone(),
        trader_path: recorded_trader_path.clone(),
        day: request.day,
        matching: request.matching.clone(),
        tick_count,
        own_trade_count,
        final_pnl_total,
        final_pnl_by_product: final_pnl_by_product.clone(),
        generated_at: metrics_generated_at.clone(),
    };

    let metrics_value = metrics_json_value(&metrics)?;
    let result_value = object(vec![
        ("run_id", Value::String(run_id.clone())),
        ("run_dir", Value::String(display_path(&run_dir))),
        ("metrics", metrics_value.clone()),
    ]);
    let result_json = sorted_json_bytes(&result_value)?;

    let artifacts = if need_submission_log || request.write_metrics || write_bundle {
        let bundle_value = object(vec![
            (
                "run",
                object(vec![
                    ("run_id", Value::String(run_id.clone())),
                    ("dataset_id", Value::String(dataset.dataset_id.clone())),
                    ("dataset_path", Value::String(recorded_dataset_path)),
                    ("trader_path", Value::String(recorded_trader_path)),
                    ("day", request.day.map(json_i64).unwrap_or(Value::Null)),
                    ("matching", matching_json_value(&request.matching)?),
                    ("generated_at", Value::String(bundle_generated_at.clone())),
                ]),
            ),
            (
                "products",
                Value::Array(
                    dataset
                        .products
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            ),
            (
                "timeline",
                Value::Array(if full_artifacts { timeline } else { Vec::new() }),
            ),
            (
                "pnl_series",
                Value::Array(if write_bundle { pnl_series } else { Vec::new() }),
            ),
        ]);
        let bundle_json = if write_bundle {
            sorted_json_bytes(&bundle_value)?
        } else {
            Vec::new()
        };
        let metrics_json = if full_artifacts || request.write_metrics {
            sorted_json_bytes(&metrics_value)?
        } else {
            Vec::new()
        };
        let activity_csv = if full_artifacts {
            build_lines_bytes(&activity_rows)
        } else {
            Vec::new()
        };
        let submission_log = if need_submission_log {
            build_submission_log(
                &run_id,
                &activity_rows,
                &sandbox_rows,
                &combined_trade_history,
            )?
        } else {
            Vec::new()
        };
        let pnl_by_product_csv = if full_artifacts {
            build_pnl_csv(&dataset.products, &bundle_value)?
        } else {
            Vec::new()
        };
        let combined_log = if full_artifacts {
            build_combined_log(&sandbox_rows, &activity_rows, &combined_trade_history)?
        } else {
            Vec::new()
        };
        let trades_csv = if full_artifacts {
            build_trades_csv(&own_trade_rows)?
        } else {
            Vec::new()
        };
        let artifact_set = ArtifactSet {
            metrics_json,
            bundle_json,
            submission_log,
            activity_csv,
            pnl_by_product_csv,
            combined_log,
            trades_csv,
        };

        if request.persist {
            write_artifacts(&run_dir, &artifact_set)?;
        } else if request.write_bundle && request.write_metrics {
            write_metrics_and_bundle(&run_dir, &artifact_set)?;
        } else if request.write_submission_log && request.write_metrics {
            write_metrics_and_submission_log(&run_dir, &artifact_set)?;
        } else if request.write_bundle {
            write_bundle_only(&run_dir, &artifact_set)?;
        } else if request.write_metrics {
            write_metrics_only(&run_dir, &artifact_set)?;
        } else if request.write_submission_log {
            write_submission_log_only(&run_dir, &artifact_set)?;
        }

        Some(artifact_set)
    } else {
        None
    };

    Ok(RunOutput {
        run_id,
        run_dir,
        metrics,
        result_json,
        artifacts,
    })
}

fn resolve_run_id(request: &RunRequest) -> Result<String> {
    if let Some(run_id) = &request.metadata_overrides.run_id {
        return Ok(run_id.clone());
    }
    if let Some(run_id) = &request.run_id {
        return Ok(run_id.clone());
    }
    Ok(now_utc_iso()?.replace(':', "-"))
}

fn resolve_generated_at(overrides: &MetadataOverrides) -> Result<String> {
    if let Some(value) = &overrides.generated_at {
        return Ok(value.clone());
    }
    now_utc_iso()
}

fn now_utc_iso() -> Result<String> {
    let format = format_description::parse("[year]-[month]-[day]T[hour]:[minute]:[second]+00:00")?;
    Ok(OffsetDateTime::now_utc().format(&format)?)
}

fn snapshot_to_book(
    snapshot: Option<&crate::model::ProductSnapshot>,
    bids: bool,
) -> Vec<BookLevel> {
    let mut out = Vec::new();
    if let Some(snapshot) = snapshot {
        let levels = if bids { &snapshot.bids } else { &snapshot.asks };
        out.reserve(levels.len());
        for level in levels {
            out.push(BookLevel {
                price: level.price,
                volume: level.volume,
            });
        }
        if bids {
            out.sort_by(|a, b| b.price.cmp(&a.price));
        } else {
            out.sort_by(|a, b| a.price.cmp(&b.price));
        }
    }
    out
}

fn enforce_position_limits(
    position: &IndexMap<String, i64>,
    orders_by_symbol: IndexMap<String, Vec<Order>>,
) -> (IndexMap<String, Vec<Order>>, Vec<String>) {
    let mut filtered = IndexMap::new();
    let mut messages = Vec::new();

    for (symbol, orders) in orders_by_symbol {
        let product_position = position.get(&symbol).copied().unwrap_or(0);
        let total_long: i64 = orders.iter().map(|order| order.quantity.max(0)).sum();
        let total_short: i64 = orders.iter().map(|order| (-order.quantity).max(0)).sum();
        let limit = position_limit(&symbol);

        if product_position + total_long > limit || product_position - total_short < -limit {
            messages.push(format!(
                "Orders for product {symbol} exceeded limit {limit}; product orders canceled for this tick"
            ));
            continue;
        }

        filtered.insert(symbol, orders);
    }

    (filtered, messages)
}

fn position_limit(symbol: &str) -> i64 {
    match symbol {
        "EMERALDS" => 80,
        "TOMATOES" => 80,
        "INTARIAN_PEPPER_ROOT" => 80,
        "ASH_COATED_OSMIUM" => 80,
        "RAINFOREST_RESIN" => 50,
        "KELP" => 50,
        "SQUID_INK" => 50,
        "CROISSANTS" => 250,
        "JAMS" => 350,
        "DJEMBES" => 60,
        "PICNIC_BASKET1" => 60,
        "PICNIC_BASKET2" => 100,
        "VOLCANIC_ROCK" => 400,
        "VOLCANIC_ROCK_VOUCHER_9500" => 200,
        "VOLCANIC_ROCK_VOUCHER_9750" => 200,
        "VOLCANIC_ROCK_VOUCHER_10000" => 200,
        "VOLCANIC_ROCK_VOUCHER_10250" => 200,
        "VOLCANIC_ROCK_VOUCHER_10500" => 200,
        "MAGNIFICENT_MACARONS" => 75,
        "HYDROGEL_PACK" => 200,
        "VELVETFRUIT_EXTRACT" => 200,
        "VEV_4000" => 300,
        "VEV_4500" => 300,
        "VEV_5000" => 300,
        "VEV_5100" => 300,
        "VEV_5200" => 300,
        "VEV_5300" => 300,
        "VEV_5400" => 300,
        "VEV_5500" => 300,
        "VEV_6000" => 300,
        "VEV_6500" => 300,
        _ if is_round5_product(symbol) => 10,
        _ => DEFAULT_POSITION_LIMIT,
    }
}

fn is_round5_product(symbol: &str) -> bool {
    [
        "GALAXY_SOUNDS_",
        "SLEEP_POD_",
        "MICROCHIP_",
        "PEBBLES_",
        "ROBOT_",
        "UV_VISOR_",
        "TRANSLATOR_",
        "PANEL_",
        "OXYGEN_SHAKE_",
        "SNACKPACK_",
    ]
    .iter()
    .any(|prefix| symbol.starts_with(prefix))
}

fn match_orders_for_symbol(
    symbol: &str,
    orders: Vec<Order>,
    bids: &mut [BookLevel],
    asks: &mut [BookLevel],
    market_trades: &[crate::model::MarketTrade],
    position: &mut IndexMap<String, i64>,
    cash_by_product: &mut IndexMap<String, f64>,
    timestamp: i64,
    config: &MatchingConfig,
    record_orders: bool,
) -> (Vec<Trade>, Vec<Trade>, Vec<FlatOrderRow>) {
    let mut own_trades = Vec::new();
    let mut order_rows = Vec::new();
    let best_bid = bids
        .iter()
        .filter(|level| level.volume > 0)
        .map(|level| level.price)
        .max();
    let best_ask = asks
        .iter()
        .filter(|level| level.volume > 0)
        .map(|level| level.price)
        .min();
    let mut market_available: Vec<Trade> = market_trades
        .iter()
        .filter(|trade| !market_trade_duplicates_touch(trade, best_bid, best_ask))
        .map(|trade| Trade {
            symbol: symbol.to_string(),
            price: trade.price,
            quantity: queue_penetration_available(trade.quantity, config.queue_penetration),
            buyer: trade.buyer.clone(),
            seller: trade.seller.clone(),
            timestamp: if trade.timestamp == 0 {
                timestamp
            } else {
                trade.timestamp
            },
        })
        .collect();
    let mut buy_queue_remaining: HashMap<i64, i64> = bids
        .iter()
        .filter(|level| level.volume > 0)
        .map(|level| (level.price, level.volume))
        .collect();
    let mut sell_queue_remaining: HashMap<i64, i64> = asks
        .iter()
        .filter(|level| level.volume > 0)
        .map(|level| (level.price, level.volume))
        .collect();

    for order in orders {
        let mut remaining = order.quantity;
        if record_orders {
            order_rows.push(FlatOrderRow {
                symbol: order.symbol.clone(),
                price: order.price,
                quantity: order.quantity,
            });
        }

        if remaining > 0 {
            for level in asks.iter_mut() {
                if remaining <= 0 {
                    break;
                }
                if level.price > order.price || level.volume <= 0 {
                    continue;
                }
                let fill = remaining.min(level.volume);
                let trade_price =
                    slippage_adjusted_price(level.price, true, config.price_slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: trade_price,
                    quantity: fill,
                    buyer: "SUBMISSION".to_string(),
                    seller: String::new(),
                    timestamp,
                });
                adjust_position(position, symbol, fill);
                adjust_cash(cash_by_product, symbol, -(trade_price as f64 * fill as f64));
                level.volume -= fill;
                remaining -= fill;
            }
        } else if remaining < 0 {
            for level in bids.iter_mut() {
                if remaining >= 0 {
                    break;
                }
                if level.price < order.price || level.volume <= 0 {
                    continue;
                }
                let fill = (-remaining).min(level.volume);
                let trade_price =
                    slippage_adjusted_price(level.price, false, config.price_slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: trade_price,
                    quantity: fill,
                    buyer: String::new(),
                    seller: "SUBMISSION".to_string(),
                    timestamp,
                });
                adjust_position(position, symbol, -fill);
                adjust_cash(cash_by_product, symbol, trade_price as f64 * fill as f64);
                level.volume -= fill;
                remaining += fill;
            }
        }

        if remaining != 0 && !config.mode_is_none() {
            for trade in &mut market_available {
                if remaining == 0 {
                    break;
                }
                if trade.quantity <= 0 {
                    continue;
                }
                if !eligible_trade_price(
                    order.price,
                    trade.price,
                    remaining,
                    &config.trade_match_mode,
                ) {
                    continue;
                }
                if remaining > 0 && trade.price == order.price {
                    if let Some(ahead) = buy_queue_remaining.get_mut(&order.price) {
                        let consumed = trade.quantity.min(*ahead);
                        trade.quantity -= consumed;
                        *ahead -= consumed;
                        if *ahead <= 0 {
                            buy_queue_remaining.remove(&order.price);
                        }
                    }
                } else if remaining < 0 && trade.price == order.price {
                    if let Some(ahead) = sell_queue_remaining.get_mut(&order.price) {
                        let consumed = trade.quantity.min(*ahead);
                        trade.quantity -= consumed;
                        *ahead -= consumed;
                        if *ahead <= 0 {
                            sell_queue_remaining.remove(&order.price);
                        }
                    }
                }
                if trade.quantity <= 0 {
                    continue;
                }

                let fill = remaining.abs().min(trade.quantity);
                let execution_price =
                    slippage_adjusted_price(order.price, remaining > 0, config.price_slippage_bps);

                if remaining > 0 {
                    own_trades.push(Trade {
                        symbol: symbol.to_string(),
                        price: execution_price,
                        quantity: fill,
                        buyer: "SUBMISSION".to_string(),
                        seller: trade.seller.clone(),
                        timestamp,
                    });
                    adjust_position(position, symbol, fill);
                    adjust_cash(
                        cash_by_product,
                        symbol,
                        -(execution_price as f64 * fill as f64),
                    );
                    remaining -= fill;
                } else {
                    own_trades.push(Trade {
                        symbol: symbol.to_string(),
                        price: execution_price,
                        quantity: fill,
                        buyer: trade.buyer.clone(),
                        seller: "SUBMISSION".to_string(),
                        timestamp,
                    });
                    adjust_position(position, symbol, -fill);
                    adjust_cash(
                        cash_by_product,
                        symbol,
                        execution_price as f64 * fill as f64,
                    );
                    remaining += fill;
                }

                trade.quantity -= fill;
            }
        }
    }

    let remaining_market = market_available
        .into_iter()
        .filter(|trade| trade.quantity > 0)
        .collect();
    (own_trades, remaining_market, order_rows)
}

fn match_orders_for_symbol_raw_csv_tape(
    symbol: &str,
    orders: Vec<Order>,
    bids: &mut [BookLevel],
    asks: &mut [BookLevel],
    market_trades: &[crate::model::MarketTrade],
    position: &mut IndexMap<String, i64>,
    cash_by_product: &mut IndexMap<String, f64>,
    timestamp: i64,
    config: &MatchingConfig,
    record_orders: bool,
) -> (Vec<Trade>, Vec<Trade>, Vec<FlatOrderRow>) {
    let mut own_trades = Vec::new();
    let mut public_market_trades = Vec::new();
    let mut order_rows = Vec::new();
    let mut resting_bids = Vec::new();
    let mut resting_asks = Vec::new();

    for order in orders {
        let mut remaining = order.quantity;
        if record_orders {
            order_rows.push(FlatOrderRow {
                symbol: order.symbol.clone(),
                price: order.price,
                quantity: order.quantity,
            });
        }

        if remaining > 0 {
            for level in asks.iter_mut() {
                if remaining <= 0 {
                    break;
                }
                if level.price > order.price || level.volume <= 0 {
                    continue;
                }
                let fill = remaining.min(level.volume);
                let trade_price =
                    slippage_adjusted_price(level.price, true, config.price_slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: trade_price,
                    quantity: fill,
                    buyer: "SUBMISSION".to_string(),
                    seller: String::new(),
                    timestamp,
                });
                adjust_position(position, symbol, fill);
                adjust_cash(cash_by_product, symbol, -(trade_price as f64 * fill as f64));
                level.volume -= fill;
                remaining -= fill;
            }
            if remaining > 0 {
                resting_bids.push(RestingSubmissionOrder {
                    price: order.price,
                    quantity: remaining,
                });
            }
        } else if remaining < 0 {
            for level in bids.iter_mut() {
                if remaining >= 0 {
                    break;
                }
                if level.price < order.price || level.volume <= 0 {
                    continue;
                }
                let fill = (-remaining).min(level.volume);
                let trade_price =
                    slippage_adjusted_price(level.price, false, config.price_slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: trade_price,
                    quantity: fill,
                    buyer: String::new(),
                    seller: "SUBMISSION".to_string(),
                    timestamp,
                });
                adjust_position(position, symbol, -fill);
                adjust_cash(cash_by_product, symbol, trade_price as f64 * fill as f64);
                level.volume -= fill;
                remaining += fill;
            }
            if remaining < 0 {
                resting_asks.push(RestingSubmissionOrder {
                    price: order.price,
                    quantity: -remaining,
                });
            }
        }
    }

    let passthrough_public_market_trades = config.raw_csv_market_trades == "full";

    for trade in market_trades {
        if trade.quantity <= 0 {
            continue;
        }
        let trade_timestamp = if trade.timestamp == 0 {
            timestamp
        } else {
            trade.timestamp
        };
        let mut synthetic_bid_remaining = trade.quantity;
        let mut synthetic_ask_remaining = trade.quantity;

        sweep_synthetic_sell_into_liquidity(
            symbol,
            trade.price,
            &mut synthetic_ask_remaining,
            bids,
            &mut resting_bids,
            position,
            cash_by_product,
            &mut own_trades,
            trade_timestamp,
            &trade.seller,
            config.price_slippage_bps,
        );
        sweep_synthetic_buy_into_liquidity(
            symbol,
            trade.price,
            &mut synthetic_bid_remaining,
            asks,
            &mut resting_asks,
            position,
            cash_by_product,
            &mut own_trades,
            trade_timestamp,
            &trade.buyer,
            config.price_slippage_bps,
        );

        let public_quantity = if passthrough_public_market_trades {
            trade.quantity
        } else {
            synthetic_bid_remaining.min(synthetic_ask_remaining)
        };
        if public_quantity > 0 {
            public_market_trades.push(Trade {
                symbol: symbol.to_string(),
                price: trade.price,
                quantity: public_quantity,
                buyer: trade.buyer.clone(),
                seller: trade.seller.clone(),
                timestamp: trade_timestamp,
            });
        }
    }

    (own_trades, public_market_trades, order_rows)
}

#[derive(Clone, Copy)]
enum RestingBidSource {
    Visible(usize),
    Submission(usize),
}

#[derive(Clone, Copy)]
enum RestingAskSource {
    Visible(usize),
    Submission(usize),
}

fn sweep_synthetic_sell_into_liquidity(
    symbol: &str,
    trade_price: i64,
    synthetic_remaining: &mut i64,
    bids: &mut [BookLevel],
    resting_bids: &mut [RestingSubmissionOrder],
    position: &mut IndexMap<String, i64>,
    cash_by_product: &mut IndexMap<String, f64>,
    own_trades: &mut Vec<Trade>,
    timestamp: i64,
    counterparty_seller: &str,
    slippage_bps: f64,
) {
    while *synthetic_remaining > 0 {
        let Some(source) = best_bid_liquidity_source(bids, resting_bids, trade_price) else {
            break;
        };
        match source {
            RestingBidSource::Visible(index) => {
                let fill = (*synthetic_remaining).min(bids[index].volume);
                bids[index].volume -= fill;
                *synthetic_remaining -= fill;
            }
            RestingBidSource::Submission(index) => {
                let fill = (*synthetic_remaining).min(resting_bids[index].quantity);
                let execution_price =
                    slippage_adjusted_price(resting_bids[index].price, true, slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: execution_price,
                    quantity: fill,
                    buyer: "SUBMISSION".to_string(),
                    seller: counterparty_seller.to_string(),
                    timestamp,
                });
                adjust_position(position, symbol, fill);
                adjust_cash(
                    cash_by_product,
                    symbol,
                    -(execution_price as f64 * fill as f64),
                );
                resting_bids[index].quantity -= fill;
                *synthetic_remaining -= fill;
            }
        }
    }
}

fn sweep_synthetic_buy_into_liquidity(
    symbol: &str,
    trade_price: i64,
    synthetic_remaining: &mut i64,
    asks: &mut [BookLevel],
    resting_asks: &mut [RestingSubmissionOrder],
    position: &mut IndexMap<String, i64>,
    cash_by_product: &mut IndexMap<String, f64>,
    own_trades: &mut Vec<Trade>,
    timestamp: i64,
    counterparty_buyer: &str,
    slippage_bps: f64,
) {
    while *synthetic_remaining > 0 {
        let Some(source) = best_ask_liquidity_source(asks, resting_asks, trade_price) else {
            break;
        };
        match source {
            RestingAskSource::Visible(index) => {
                let fill = (*synthetic_remaining).min(asks[index].volume);
                asks[index].volume -= fill;
                *synthetic_remaining -= fill;
            }
            RestingAskSource::Submission(index) => {
                let fill = (*synthetic_remaining).min(resting_asks[index].quantity);
                let execution_price =
                    slippage_adjusted_price(resting_asks[index].price, false, slippage_bps);
                own_trades.push(Trade {
                    symbol: symbol.to_string(),
                    price: execution_price,
                    quantity: fill,
                    buyer: counterparty_buyer.to_string(),
                    seller: "SUBMISSION".to_string(),
                    timestamp,
                });
                adjust_position(position, symbol, -fill);
                adjust_cash(
                    cash_by_product,
                    symbol,
                    execution_price as f64 * fill as f64,
                );
                resting_asks[index].quantity -= fill;
                *synthetic_remaining -= fill;
            }
        }
    }
}

fn best_bid_liquidity_source(
    bids: &[BookLevel],
    resting_bids: &[RestingSubmissionOrder],
    min_price: i64,
) -> Option<RestingBidSource> {
    let visible = best_visible_bid_index(bids, min_price);
    let submission = best_resting_bid_index(resting_bids, min_price);
    match (visible, submission) {
        (Some(visible_index), Some(submission_index)) => {
            let visible_price = bids[visible_index].price;
            let submission_price = resting_bids[submission_index].price;
            if visible_price >= submission_price {
                Some(RestingBidSource::Visible(visible_index))
            } else {
                Some(RestingBidSource::Submission(submission_index))
            }
        }
        (Some(index), None) => Some(RestingBidSource::Visible(index)),
        (None, Some(index)) => Some(RestingBidSource::Submission(index)),
        (None, None) => None,
    }
}

fn best_ask_liquidity_source(
    asks: &[BookLevel],
    resting_asks: &[RestingSubmissionOrder],
    max_price: i64,
) -> Option<RestingAskSource> {
    let visible = best_visible_ask_index(asks, max_price);
    let submission = best_resting_ask_index(resting_asks, max_price);
    match (visible, submission) {
        (Some(visible_index), Some(submission_index)) => {
            let visible_price = asks[visible_index].price;
            let submission_price = resting_asks[submission_index].price;
            if visible_price <= submission_price {
                Some(RestingAskSource::Visible(visible_index))
            } else {
                Some(RestingAskSource::Submission(submission_index))
            }
        }
        (Some(index), None) => Some(RestingAskSource::Visible(index)),
        (None, Some(index)) => Some(RestingAskSource::Submission(index)),
        (None, None) => None,
    }
}

fn best_visible_bid_index(bids: &[BookLevel], min_price: i64) -> Option<usize> {
    bids.iter()
        .enumerate()
        .find(|(_, level)| level.volume > 0 && level.price >= min_price)
        .map(|(index, _)| index)
}

fn best_visible_ask_index(asks: &[BookLevel], max_price: i64) -> Option<usize> {
    asks.iter()
        .enumerate()
        .find(|(_, level)| level.volume > 0 && level.price <= max_price)
        .map(|(index, _)| index)
}

fn best_resting_bid_index(
    resting_bids: &[RestingSubmissionOrder],
    min_price: i64,
) -> Option<usize> {
    let mut best = None;
    let mut best_price = i64::MIN;
    for (index, order) in resting_bids.iter().enumerate() {
        if order.quantity <= 0 || order.price < min_price {
            continue;
        }
        if best.is_none() || order.price > best_price {
            best = Some(index);
            best_price = order.price;
        }
    }
    best
}

fn best_resting_ask_index(
    resting_asks: &[RestingSubmissionOrder],
    max_price: i64,
) -> Option<usize> {
    let mut best = None;
    let mut best_price = i64::MAX;
    for (index, order) in resting_asks.iter().enumerate() {
        if order.quantity <= 0 || order.price > max_price {
            continue;
        }
        if best.is_none() || order.price < best_price {
            best = Some(index);
            best_price = order.price;
        }
    }
    best
}

fn market_trade_duplicates_touch(
    trade: &crate::model::MarketTrade,
    best_bid: Option<i64>,
    best_ask: Option<i64>,
) -> bool {
    if trade.buyer == "SUBMISSION" {
        if let Some(best_ask) = best_ask {
            if trade.price >= best_ask {
                return true;
            }
        }
    }
    if trade.seller == "SUBMISSION" {
        if let Some(best_bid) = best_bid {
            if trade.price <= best_bid {
                return true;
            }
        }
    }
    false
}

fn queue_penetration_available(quantity: i64, queue_penetration: f64) -> i64 {
    let raw = quantity as f64 * queue_penetration.max(0.0);
    let mut available = python_round_to_i64(raw);
    if quantity > 0 && queue_penetration > 0.0 && available == 0 {
        available = 1;
    }
    available.max(0)
}

fn eligible_trade_price(order_price: i64, trade_price: i64, quantity: i64, mode: &str) -> bool {
    if mode == "none" {
        return false;
    }
    if quantity > 0 {
        if mode == "all" {
            return trade_price <= order_price;
        }
        return trade_price < order_price;
    }
    if quantity < 0 {
        if mode == "all" {
            return trade_price >= order_price;
        }
        return trade_price > order_price;
    }
    false
}

fn slippage_adjusted_price(price: i64, is_buy: bool, bps: f64) -> i64 {
    if bps <= 0.0 {
        return price;
    }
    let factor = 1.0 + (bps / 10_000.0);
    let adjusted = if is_buy {
        price as f64 * factor
    } else {
        price as f64 / factor
    };
    python_round_to_i64(adjusted)
}

fn python_round_to_i64(value: f64) -> i64 {
    value.round_ties_even() as i64
}

fn python_round_to_digits(value: f64, digits: i32) -> f64 {
    let factor = 10_f64.powi(digits);
    (value * factor).round_ties_even() / factor
}

fn adjust_position(position: &mut IndexMap<String, i64>, symbol: &str, delta: i64) {
    let entry = position.entry(symbol.to_string()).or_insert(0);
    *entry += delta;
}

fn adjust_cash(cash_by_product: &mut IndexMap<String, f64>, symbol: &str, delta: f64) {
    let entry = cash_by_product.entry(symbol.to_string()).or_insert(0.0);
    *entry += delta;
}

fn truncate_logs(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= LOG_CHAR_LIMIT {
            break;
        }
        out.push(ch);
    }
    out
}

fn resolve_stable_mark(snapshot: Option<&crate::model::ProductSnapshot>) -> Option<f64> {
    let Some(snapshot) = snapshot else {
        return None;
    };

    let best_bid = snapshot.bids.first().map(|level| level.price as f64);
    let best_ask = snapshot.asks.first().map(|level| level.price as f64);
    let positive_mid = snapshot.mid_price.filter(|price| *price > 0.0);

    match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => positive_mid.or(Some((bid + ask) / 2.0)),
        _ => None,
    }
}

fn resolve_mark_price(
    snapshot: Option<&crate::model::ProductSnapshot>,
    previous_stable_mark: Option<f64>,
) -> Option<f64> {
    if let Some(stable_mark) = resolve_stable_mark(snapshot) {
        return Some(stable_mark);
    }

    let Some(snapshot) = snapshot else {
        return previous_stable_mark;
    };

    let positive_mid = snapshot.mid_price.filter(|price| *price > 0.0);
    let best_bid = snapshot.bids.first().map(|level| level.price as f64);
    let best_ask = snapshot.asks.first().map(|level| level.price as f64);

    if previous_stable_mark.is_some() {
        previous_stable_mark
    } else {
        positive_mid.or(best_bid).or(best_ask)
    }
}

fn build_timeline_row(
    tick: &TickSnapshot,
    orders_flat: &[FlatOrderRow],
    own_trades_tick: &IndexMap<String, Vec<Trade>>,
    market_trades_next: &IndexMap<String, Vec<Trade>>,
    position: &IndexMap<String, i64>,
    mark_prices: &IndexMap<String, f64>,
    pnl_by_product: &IndexMap<String, f64>,
    pnl_total: f64,
    sandbox_log: &str,
    algorithm_logs: &str,
    conversions: i64,
    trader_data: &str,
) -> Result<Value> {
    let mut products = IndexMap::new();
    for (product, snapshot) in &tick.products {
        products.insert(
            product.clone(),
            object(vec![
                (
                    "bids",
                    Value::Array(
                        snapshot
                            .bids
                            .iter()
                            .map(|level| {
                                object(vec![
                                    ("price", json_i64(level.price)),
                                    ("volume", json_i64(level.volume)),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "asks",
                    Value::Array(
                        snapshot
                            .asks
                            .iter()
                            .map(|level| {
                                object(vec![
                                    ("price", json_i64(level.price)),
                                    ("volume", json_i64(level.volume)),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "mid_price",
                    mark_prices
                        .get(product)
                        .copied()
                        .map(json_f64)
                        .transpose()?
                        .unwrap_or(Value::Null),
                ),
            ]),
        );
    }

    let own_trades = own_trades_tick
        .values()
        .flat_map(|rows| rows.iter())
        .map(trade_json)
        .collect();
    let market_trades = market_trades_next
        .values()
        .flat_map(|rows| rows.iter())
        .map(trade_json)
        .collect();

    Ok(object(vec![
        ("timestamp", json_i64(tick.timestamp)),
        ("day", tick.day.map(json_i64).unwrap_or(Value::Null)),
        ("products", index_object(&products)),
        (
            "orders",
            Value::Array(
                orders_flat
                    .iter()
                    .map(|row| {
                        object(vec![
                            ("symbol", Value::String(row.symbol.clone())),
                            ("price", json_i64(row.price)),
                            ("quantity", json_i64(row.quantity)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("own_trades", Value::Array(own_trades)),
        ("market_trades", Value::Array(market_trades)),
        ("position", index_object(&indexmap_i64_to_json(position))),
        ("pnl_total", json_f64(pnl_total)?),
        (
            "pnl_by_product",
            index_object(&indexmap_f64_to_json(pnl_by_product)?),
        ),
        ("sandbox_logs", Value::String(sandbox_log.to_string())),
        ("algorithm_logs", Value::String(algorithm_logs.to_string())),
        ("conversions", json_i64(conversions)),
        ("trader_data", Value::String(trader_data.to_string())),
    ]))
}

fn format_activity_row(
    tick: &TickSnapshot,
    product: &str,
    snapshot: Option<&crate::model::ProductSnapshot>,
    mark_price: Option<f64>,
    pnl: f64,
) -> String {
    let bid_prices: Vec<String> = snapshot
        .map(|row| {
            row.bids
                .iter()
                .map(|level| level.price.to_string())
                .collect()
        })
        .unwrap_or_default();
    let bid_volumes: Vec<String> = snapshot
        .map(|row| {
            row.bids
                .iter()
                .map(|level| level.volume.to_string())
                .collect()
        })
        .unwrap_or_default();
    let ask_prices: Vec<String> = snapshot
        .map(|row| {
            row.asks
                .iter()
                .map(|level| level.price.to_string())
                .collect()
        })
        .unwrap_or_default();
    let ask_volumes: Vec<String> = snapshot
        .map(|row| {
            row.asks
                .iter()
                .map(|level| level.volume.to_string())
                .collect()
        })
        .unwrap_or_default();
    let rounded_pnl = python_round_to_digits(pnl, 6);

    [
        tick.day.map(|value| value.to_string()).unwrap_or_default(),
        tick.timestamp.to_string(),
        product.to_string(),
        get_or_empty(&bid_prices, 0),
        get_or_empty(&bid_volumes, 0),
        get_or_empty(&bid_prices, 1),
        get_or_empty(&bid_volumes, 1),
        get_or_empty(&bid_prices, 2),
        get_or_empty(&bid_volumes, 2),
        get_or_empty(&ask_prices, 0),
        get_or_empty(&ask_volumes, 0),
        get_or_empty(&ask_prices, 1),
        get_or_empty(&ask_volumes, 1),
        get_or_empty(&ask_prices, 2),
        get_or_empty(&ask_volumes, 2),
        mark_price.map(python_float_string).unwrap_or_default(),
        python_float_string(rounded_pnl),
    ]
    .join(";")
}

fn get_or_empty(values: &[String], idx: usize) -> String {
    values.get(idx).cloned().unwrap_or_default()
}

fn build_lines_bytes(lines: &[String]) -> Vec<u8> {
    let mut out = lines.join("\n").into_bytes();
    out.push(b'\n');
    out
}

fn build_pnl_csv(products: &[String], bundle_value: &Value) -> Result<Vec<u8>> {
    let pnl_series = bundle_value
        .get("pnl_series")
        .and_then(Value::as_array)
        .context("bundle missing pnl_series")?;
    let mut lines = Vec::new();
    let mut header = vec!["timestamp".to_string(), "total".to_string()];
    header.extend(products.iter().cloned());
    lines.push(header.join(";"));

    for row in pnl_series {
        let timestamp = row
            .get("timestamp")
            .and_then(Value::as_i64)
            .context("pnl_series row missing timestamp")?;
        let total = row
            .get("total")
            .and_then(Value::as_f64)
            .context("pnl_series row missing total")?;
        let by_product = row
            .get("by_product")
            .and_then(Value::as_object)
            .context("pnl_series row missing by_product")?;
        let mut fields = vec![timestamp.to_string(), python_float_string(total)];
        for product in products {
            let value = by_product
                .get(product)
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            fields.push(python_float_string(value));
        }
        lines.push(fields.join(";"));
    }

    Ok(build_lines_bytes(&lines))
}

fn build_combined_log(
    sandbox_rows: &[Value],
    activity_rows: &[String],
    trade_history: &[Value],
) -> Result<Vec<u8>> {
    let mut out = String::new();
    out.push_str("Sandbox logs:\n");
    for row in sandbox_rows {
        out.push_str(&String::from_utf8(pretty_json_bytes(row)?)?);
    }
    out.push('\n');
    out.push_str("Activities log:\n");
    out.push_str(&activity_rows.join("\n"));
    out.push_str("\n\nTrade History:\n");
    out.push_str(&String::from_utf8(pretty_json_bytes(&Value::Array(
        trade_history.to_vec(),
    ))?)?);
    Ok(out.into_bytes())
}

fn build_submission_log(
    run_id: &str,
    activity_rows: &[String],
    sandbox_rows: &[Value],
    trade_history: &[Value],
) -> Result<Vec<u8>> {
    let payload = object(vec![
        ("submissionId", Value::String(run_id.to_string())),
        ("activitiesLog", Value::String(activity_rows.join("\n"))),
        ("logs", Value::Array(sandbox_rows.to_vec())),
        ("tradeHistory", Value::Array(trade_history.to_vec())),
    ]);
    Ok(pretty_json_bytes(&payload)?)
}

fn build_trades_csv(own_trade_rows: &[Value]) -> Result<Vec<u8>> {
    let mut lines = vec!["timestamp;buyer;seller;symbol;currency;price;quantity".to_string()];
    for row in own_trade_rows {
        let object = row
            .as_object()
            .context("trade row should be a JSON object")?;
        lines.push(
            [
                object
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .context("trade row missing timestamp")?
                    .to_string(),
                object
                    .get("buyer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                object
                    .get("seller")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                object
                    .get("symbol")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                object
                    .get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                object
                    .get("price")
                    .and_then(Value::as_i64)
                    .context("trade row missing price")?
                    .to_string(),
                object
                    .get("quantity")
                    .and_then(Value::as_i64)
                    .context("trade row missing quantity")?
                    .to_string(),
            ]
            .join(";"),
        );
    }
    Ok(build_lines_bytes(&lines))
}

fn write_artifacts(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("metrics.json"), &artifacts.metrics_json)?;
    fs::write(run_dir.join("bundle.json"), &artifacts.bundle_json)?;
    fs::write(run_dir.join("submission.log"), &artifacts.submission_log)?;
    fs::write(run_dir.join("activity.csv"), &artifacts.activity_csv)?;
    fs::write(
        run_dir.join("pnl_by_product.csv"),
        &artifacts.pnl_by_product_csv,
    )?;
    fs::write(run_dir.join("combined.log"), &artifacts.combined_log)?;
    fs::write(run_dir.join("trades.csv"), &artifacts.trades_csv)?;
    Ok(())
}

fn write_submission_log_only(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("submission.log"), &artifacts.submission_log)?;
    Ok(())
}

fn write_bundle_only(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("bundle.json"), &artifacts.bundle_json)?;
    Ok(())
}

fn write_metrics_and_bundle(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("metrics.json"), &artifacts.metrics_json)?;
    fs::write(run_dir.join("bundle.json"), &artifacts.bundle_json)?;
    Ok(())
}

fn write_metrics_and_submission_log(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("metrics.json"), &artifacts.metrics_json)?;
    fs::write(run_dir.join("submission.log"), &artifacts.submission_log)?;
    Ok(())
}

fn write_metrics_only(run_dir: &Path, artifacts: &ArtifactSet) -> Result<()> {
    fs::write(run_dir.join("metrics.json"), &artifacts.metrics_json)?;
    Ok(())
}

fn metrics_json_value(metrics: &RunMetrics) -> Result<Value> {
    Ok(object(vec![
        ("run_id", Value::String(metrics.run_id.clone())),
        ("dataset_id", Value::String(metrics.dataset_id.clone())),
        ("dataset_path", Value::String(metrics.dataset_path.clone())),
        ("trader_path", Value::String(metrics.trader_path.clone())),
        ("day", metrics.day.map(json_i64).unwrap_or(Value::Null)),
        ("matching", matching_json_value(&metrics.matching)?),
        ("tick_count", json_usize(metrics.tick_count)),
        ("own_trade_count", json_usize(metrics.own_trade_count)),
        ("final_pnl_total", json_f64(metrics.final_pnl_total)?),
        (
            "final_pnl_by_product",
            index_object(&indexmap_f64_to_json(&metrics.final_pnl_by_product)?),
        ),
        ("generated_at", Value::String(metrics.generated_at.clone())),
    ]))
}

fn matching_json_value(config: &MatchingConfig) -> Result<Value> {
    Ok(object(vec![
        (
            "trade_match_mode",
            Value::String(config.trade_match_mode.clone()),
        ),
        ("queue_penetration", json_f64(config.queue_penetration)?),
        ("price_slippage_bps", json_f64(config.price_slippage_bps)?),
        (
            "raw_csv_market_trades",
            Value::String(config.raw_csv_market_trades.clone()),
        ),
    ]))
}

fn trade_json(trade: &Trade) -> Value {
    object(vec![
        ("symbol", Value::String(trade.symbol.clone())),
        ("price", json_i64(trade.price)),
        ("quantity", json_i64(trade.quantity)),
        ("buyer", Value::String(trade.buyer.clone())),
        ("seller", Value::String(trade.seller.clone())),
        ("timestamp", json_i64(trade.timestamp)),
    ])
}

fn trade_history_json(trade: &Trade, day: Option<i64>) -> Value {
    object(vec![
        ("day", day.map(json_i64).unwrap_or(Value::Null)),
        ("timestamp", json_i64(trade.timestamp)),
        ("buyer", Value::String(trade.buyer.clone())),
        ("seller", Value::String(trade.seller.clone())),
        ("symbol", Value::String(trade.symbol.clone())),
        ("currency", Value::String("SEASHELLS".to_string())),
        ("price", json_i64(trade.price)),
        ("quantity", json_i64(trade.quantity)),
    ])
}

fn indexmap_i64_to_json(values: &IndexMap<String, i64>) -> IndexMap<String, Value> {
    let mut out = IndexMap::new();
    for (key, value) in values {
        out.insert(key.clone(), json_i64(*value));
    }
    out
}

fn indexmap_f64_to_json(values: &IndexMap<String, f64>) -> Result<IndexMap<String, Value>> {
    let mut out = IndexMap::new();
    for (key, value) in values {
        out.insert(key.clone(), json_f64(*value)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        BookLevel, display_path, eligible_trade_price, enforce_position_limits,
        market_trade_duplicates_touch, match_orders_for_symbol,
        match_orders_for_symbol_raw_csv_tape, project_root, python_round_to_digits,
        python_round_to_i64, queue_penetration_available, run_backtest, slippage_adjusted_price,
        position_limit,
    };
    use crate::model::{
        MarketTrade, MatchingConfig, NormalizedDataset, ObservationState, Order, OrderBookLevel,
        ProductSnapshot, RunRequest, TickSnapshot,
    };
    use indexmap::IndexMap;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn queue_penetration_uses_bankers_rounding() {
        assert_eq!(queue_penetration_available(5, 0.5), 2);
        assert_eq!(queue_penetration_available(3, 0.5), 2);
    }

    #[test]
    fn slippage_adjustment_matches_expected_rounding() {
        assert_eq!(slippage_adjusted_price(10005, true, 50.0), 10055);
        assert_eq!(slippage_adjusted_price(10005, false, 50.0), 9955);
    }

    #[test]
    fn trade_match_modes_follow_python_logic() {
        assert!(eligible_trade_price(100, 100, 1, "all"));
        assert!(!eligible_trade_price(100, 100, 1, "worse"));
        assert!(eligible_trade_price(100, 101, -1, "all"));
        assert!(!eligible_trade_price(100, 100, -1, "worse"));
    }

    #[test]
    fn market_trade_duplicate_filter_skips_touch_submission_rows() {
        let trade = MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
            buyer: String::new(),
            seller: "SUBMISSION".to_string(),
            timestamp: 0,
        };
        assert!(market_trade_duplicates_touch(&trade, Some(100), Some(104)));

        let off_touch = MarketTrade {
            price: 101,
            ..trade
        };
        assert!(!market_trade_duplicates_touch(
            &off_touch,
            Some(100),
            Some(104)
        ));

        let through_ask = MarketTrade {
            price: 106,
            quantity: 4,
            buyer: "SUBMISSION".to_string(),
            seller: String::new(),
            timestamp: 0,
            symbol: "TOMATOES".to_string(),
        };
        assert!(market_trade_duplicates_touch(
            &through_ask,
            Some(100),
            Some(104)
        ));
    }

    #[test]
    fn same_price_market_trade_respects_visible_queue_ahead() {
        let orders = vec![Order {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
        }];
        let mut bids = vec![
            BookLevel {
                price: 100,
                volume: 5,
            },
            BookLevel {
                price: 99,
                volume: 7,
            },
        ];
        let mut asks = vec![BookLevel {
            price: 105,
            volume: 5,
        }];
        let market_trades = vec![MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 3,
            buyer: String::new(),
            seller: "BOT".to_string(),
            timestamp: 0,
        }];
        let mut position = IndexMap::new();
        let mut cash = IndexMap::new();
        let config = MatchingConfig::default();
        let (own_trades, remaining_market, order_rows) = match_orders_for_symbol(
            "TOMATOES",
            orders,
            &mut bids,
            &mut asks,
            &market_trades,
            &mut position,
            &mut cash,
            0,
            &config,
            true,
        );

        assert!(own_trades.is_empty());
        assert!(remaining_market.is_empty());
        assert_eq!(order_rows.len(), 1);
        assert_eq!(order_rows[0].symbol, "TOMATOES");
        assert_eq!(order_rows[0].price, 100);
        assert_eq!(order_rows[0].quantity, 4);
    }

    #[test]
    fn raw_csv_same_price_priority_prefers_visible_book_before_submission_remainder() {
        let orders = vec![Order {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 5,
        }];
        let mut bids = vec![BookLevel {
            price: 100,
            volume: 2,
        }];
        let mut asks = vec![BookLevel {
            price: 105,
            volume: 5,
        }];
        let market_trades = vec![MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 6,
            buyer: "BOT_BUYER".to_string(),
            seller: "BOT_SELLER".to_string(),
            timestamp: 0,
        }];
        let mut position = IndexMap::new();
        let mut cash = IndexMap::new();
        let config = MatchingConfig::default();

        let (own_trades, public_market_trades, _) = match_orders_for_symbol_raw_csv_tape(
            "TOMATOES",
            orders,
            &mut bids,
            &mut asks,
            &market_trades,
            &mut position,
            &mut cash,
            0,
            &config,
            false,
        );

        assert_eq!(own_trades.len(), 1);
        assert_eq!(own_trades[0].price, 100);
        assert_eq!(own_trades[0].quantity, 4);
        assert!(public_market_trades.is_empty());
        assert_eq!(position.get("TOMATOES").copied(), Some(4));
    }

    #[test]
    fn raw_csv_visible_bid_can_fully_block_submission_bid_fill() {
        let orders = vec![Order {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
        }];
        let mut bids = vec![BookLevel {
            price: 100,
            volume: 6,
        }];
        let mut asks = vec![];
        let market_trades = vec![MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
            buyer: "BOT_BUYER".to_string(),
            seller: "BOT_SELLER".to_string(),
            timestamp: 0,
        }];
        let mut position = IndexMap::new();
        let mut cash = IndexMap::new();
        let config = MatchingConfig::default();

        let (own_trades, public_market_trades, _) = match_orders_for_symbol_raw_csv_tape(
            "TOMATOES",
            orders,
            &mut bids,
            &mut asks,
            &market_trades,
            &mut position,
            &mut cash,
            0,
            &config,
            false,
        );

        assert!(own_trades.is_empty());
        assert!(public_market_trades.is_empty());
        assert_eq!(position.get("TOMATOES").copied(), None);
    }

    #[test]
    fn raw_csv_visible_ask_can_fully_block_submission_ask_fill() {
        let orders = vec![Order {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: -4,
        }];
        let mut bids = vec![];
        let mut asks = vec![BookLevel {
            price: 100,
            volume: 6,
        }];
        let market_trades = vec![MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
            buyer: "BOT_BUYER".to_string(),
            seller: "BOT_SELLER".to_string(),
            timestamp: 0,
        }];
        let mut position = IndexMap::new();
        let mut cash = IndexMap::new();
        let config = MatchingConfig::default();

        let (own_trades, public_market_trades, _) = match_orders_for_symbol_raw_csv_tape(
            "TOMATOES",
            orders,
            &mut bids,
            &mut asks,
            &market_trades,
            &mut position,
            &mut cash,
            0,
            &config,
            false,
        );

        assert!(own_trades.is_empty());
        assert!(public_market_trades.is_empty());
        assert_eq!(position.get("TOMATOES").copied(), None);
    }

    #[test]
    fn non_csv_trade_history_replay_keeps_queue_penetration_behavior() {
        let orders = vec![Order {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 4,
        }];
        let mut bids = Vec::new();
        let mut asks = vec![BookLevel {
            price: 105,
            volume: 1,
        }];
        let market_trades = vec![MarketTrade {
            symbol: "TOMATOES".to_string(),
            price: 100,
            quantity: 5,
            buyer: "BOT_BUYER".to_string(),
            seller: "BOT_SELLER".to_string(),
            timestamp: 0,
        }];
        let mut position = IndexMap::new();
        let mut cash = IndexMap::new();
        let config = MatchingConfig {
            queue_penetration: 0.5,
            ..MatchingConfig::default()
        };

        let (own_trades, public_market_trades, _) = match_orders_for_symbol(
            "TOMATOES",
            orders,
            &mut bids,
            &mut asks,
            &market_trades,
            &mut position,
            &mut cash,
            0,
            &config,
            false,
        );

        assert_eq!(own_trades.len(), 1);
        assert_eq!(own_trades[0].price, 100);
        assert_eq!(own_trades[0].quantity, 2);
        assert!(public_market_trades.is_empty());
        assert_eq!(position.get("TOMATOES").copied(), Some(2));
    }

    #[test]
    fn product_specific_limits_allow_positions_up_to_cap() {
        let position =
            IndexMap::from([("EMERALDS".to_string(), 75), ("TOMATOES".to_string(), -75)]);
        let orders_by_symbol = IndexMap::from([
            (
                "EMERALDS".to_string(),
                vec![Order {
                    symbol: "EMERALDS".to_string(),
                    price: 10_000,
                    quantity: 5,
                }],
            ),
            (
                "TOMATOES".to_string(),
                vec![Order {
                    symbol: "TOMATOES".to_string(),
                    price: 5_000,
                    quantity: -5,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, orders_by_symbol);

        assert!(messages.is_empty());
        assert_eq!(filtered["EMERALDS"][0].quantity, 5);
        assert_eq!(filtered["TOMATOES"][0].quantity, -5);
    }

    #[test]
    fn product_specific_limits_reject_orders_beyond_cap() {
        let position =
            IndexMap::from([("EMERALDS".to_string(), 75), ("TOMATOES".to_string(), -75)]);
        let orders_by_symbol = IndexMap::from([
            (
                "EMERALDS".to_string(),
                vec![Order {
                    symbol: "EMERALDS".to_string(),
                    price: 10_000,
                    quantity: 6,
                }],
            ),
            (
                "TOMATOES".to_string(),
                vec![Order {
                    symbol: "TOMATOES".to_string(),
                    price: 5_000,
                    quantity: -6,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, orders_by_symbol);

        assert!(filtered.is_empty());
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("EMERALDS"));
        assert!(messages[0].contains("80"));
        assert!(messages[1].contains("TOMATOES"));
        assert!(messages[1].contains("80"));
    }

    #[test]
    fn round3_products_use_wiki_position_limits() {
        let position = IndexMap::from([
            ("HYDROGEL_PACK".to_string(), 199),
            ("VELVETFRUIT_EXTRACT".to_string(), -199),
            ("VEV_5200".to_string(), 299),
        ]);
        let allowed_orders = IndexMap::from([
            (
                "HYDROGEL_PACK".to_string(),
                vec![Order {
                    symbol: "HYDROGEL_PACK".to_string(),
                    price: 10_000,
                    quantity: 1,
                }],
            ),
            (
                "VELVETFRUIT_EXTRACT".to_string(),
                vec![Order {
                    symbol: "VELVETFRUIT_EXTRACT".to_string(),
                    price: 5_250,
                    quantity: -1,
                }],
            ),
            (
                "VEV_5200".to_string(),
                vec![Order {
                    symbol: "VEV_5200".to_string(),
                    price: 100,
                    quantity: 1,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, allowed_orders);

        assert!(messages.is_empty());
        assert_eq!(filtered.len(), 3);

        let rejected_orders = IndexMap::from([
            (
                "HYDROGEL_PACK".to_string(),
                vec![Order {
                    symbol: "HYDROGEL_PACK".to_string(),
                    price: 10_000,
                    quantity: 2,
                }],
            ),
            (
                "VELVETFRUIT_EXTRACT".to_string(),
                vec![Order {
                    symbol: "VELVETFRUIT_EXTRACT".to_string(),
                    price: 5_250,
                    quantity: -2,
                }],
            ),
            (
                "VEV_5200".to_string(),
                vec![Order {
                    symbol: "VEV_5200".to_string(),
                    price: 100,
                    quantity: 2,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, rejected_orders);

        assert!(filtered.is_empty());
        assert_eq!(messages.len(), 3);
        assert!(messages[0].contains("200"));
        assert!(messages[1].contains("200"));
        assert!(messages[2].contains("300"));
    }

    #[test]
    fn round5_products_use_limit_ten() {
        let round5_products = [
            "GALAXY_SOUNDS_DARK_MATTER",
            "GALAXY_SOUNDS_BLACK_HOLES",
            "GALAXY_SOUNDS_PLANETARY_RINGS",
            "GALAXY_SOUNDS_SOLAR_WINDS",
            "GALAXY_SOUNDS_SOLAR_FLAMES",
            "SLEEP_POD_SUEDE",
            "SLEEP_POD_LAMB_WOOL",
            "SLEEP_POD_POLYESTER",
            "SLEEP_POD_NYLON",
            "SLEEP_POD_COTTON",
            "MICROCHIP_CIRCLE",
            "MICROCHIP_OVAL",
            "MICROCHIP_SQUARE",
            "MICROCHIP_RECTANGLE",
            "MICROCHIP_TRIANGLE",
            "PEBBLES_XS",
            "PEBBLES_S",
            "PEBBLES_M",
            "PEBBLES_L",
            "PEBBLES_XL",
            "ROBOT_VACUUMING",
            "ROBOT_MOPPING",
            "ROBOT_DISHES",
            "ROBOT_LAUNDRY",
            "ROBOT_IRONING",
            "UV_VISOR_YELLOW",
            "UV_VISOR_AMBER",
            "UV_VISOR_ORANGE",
            "UV_VISOR_RED",
            "UV_VISOR_MAGENTA",
            "TRANSLATOR_SPACE_GRAY",
            "TRANSLATOR_ASTRO_BLACK",
            "TRANSLATOR_ECLIPSE_CHARCOAL",
            "TRANSLATOR_GRAPHITE_MIST",
            "TRANSLATOR_VOID_BLUE",
            "PANEL_1X2",
            "PANEL_2X2",
            "PANEL_1X4",
            "PANEL_2X4",
            "PANEL_4X4",
            "OXYGEN_SHAKE_MORNING_BREATH",
            "OXYGEN_SHAKE_EVENING_BREATH",
            "OXYGEN_SHAKE_MINT",
            "OXYGEN_SHAKE_CHOCOLATE",
            "OXYGEN_SHAKE_GARLIC",
            "SNACKPACK_CHOCOLATE",
            "SNACKPACK_VANILLA",
            "SNACKPACK_PISTACHIO",
            "SNACKPACK_STRAWBERRY",
            "SNACKPACK_RASPBERRY",
        ];

        for product in round5_products {
            assert_eq!(position_limit(product), 10, "{product} should have limit 10");
        }

        let position = IndexMap::from([
            ("GALAXY_SOUNDS_DARK_MATTER".to_string(), 9),
            ("SNACKPACK_RASPBERRY".to_string(), -9),
        ]);
        let allowed_orders = IndexMap::from([
            (
                "GALAXY_SOUNDS_DARK_MATTER".to_string(),
                vec![Order {
                    symbol: "GALAXY_SOUNDS_DARK_MATTER".to_string(),
                    price: 10_000,
                    quantity: 1,
                }],
            ),
            (
                "SNACKPACK_RASPBERRY".to_string(),
                vec![Order {
                    symbol: "SNACKPACK_RASPBERRY".to_string(),
                    price: 10_000,
                    quantity: -1,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, allowed_orders);
        assert!(messages.is_empty());
        assert_eq!(filtered.len(), 2);

        let rejected_orders = IndexMap::from([
            (
                "GALAXY_SOUNDS_DARK_MATTER".to_string(),
                vec![Order {
                    symbol: "GALAXY_SOUNDS_DARK_MATTER".to_string(),
                    price: 10_000,
                    quantity: 2,
                }],
            ),
            (
                "SNACKPACK_RASPBERRY".to_string(),
                vec![Order {
                    symbol: "SNACKPACK_RASPBERRY".to_string(),
                    price: 10_000,
                    quantity: -2,
                }],
            ),
        ]);

        let (filtered, messages) = enforce_position_limits(&position, rejected_orders);
        assert!(filtered.is_empty());
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("10"));
        assert!(messages[1].contains("10"));
    }

    #[test]
    fn log_only_mode_writes_submission_log_and_metrics() {
        let unique = format!(
            "runner-log-only-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(unique);
        let request = RunRequest {
            trader_file: project_root().join("traders/latest_trader.py"),
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: None,
            day: Some(-1),
            matching: MatchingConfig::default(),
            run_id: Some("log-only-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: false,
            write_submission_log: true,
            materialize_artifacts: false,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");

        assert!(output.run_dir.join("metrics.json").is_file());
        assert!(output.run_dir.join("submission.log").is_file());

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn dataset_override_carries_position_across_days() {
        let unique = format!(
            "runner-carry-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("carry_probe_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        seen = int(state.position.get("EMERALDS", 0))
        print(f"seen={seen}")
        orders = {}
        if seen == 0:
            orders["EMERALDS"] = [Order("EMERALDS", 10000, 1)]
        return orders, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "carry-test".to_string(),
            source: "test".to_string(),
            products: vec!["EMERALDS".to_string()],
            metadata: IndexMap::new(),
            ticks: vec![
                TickSnapshot {
                    timestamp: 0,
                    day: Some(-2),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![OrderBookLevel {
                                price: 9999,
                                volume: 1,
                            }],
                            asks: vec![OrderBookLevel {
                                price: 10000,
                                volume: 1,
                            }],
                            mid_price: Some(9999.5),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
                TickSnapshot {
                    timestamp: 0,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![OrderBookLevel {
                                price: 9999,
                                volume: 1,
                            }],
                            asks: vec![OrderBookLevel {
                                price: 10000,
                                volume: 1,
                            }],
                            mid_price: Some(9999.5),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
            ],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("carry-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: false,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("carry backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");

        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0]["day"].as_i64(), Some(-2));
        assert_eq!(timeline[1]["day"].as_i64(), Some(-1));
        assert_eq!(timeline[0]["position"]["EMERALDS"].as_i64(), Some(1));
        assert_eq!(timeline[1]["position"]["EMERALDS"].as_i64(), Some(1));
        assert_eq!(timeline[0]["algorithm_logs"].as_str(), Some("seen=0"));
        assert_eq!(timeline[1]["algorithm_logs"].as_str(), Some("seen=1"));

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn empty_book_carries_forward_last_mark_for_pnl_and_activity_log() {
        let unique = format!(
            "runner-empty-book-mark-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("empty_book_mark_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        if state.timestamp == 0:
            return {"EMERALDS": [Order("EMERALDS", 10000, 1)]}, 0, ""
        return {}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "empty-book-mark-test".to_string(),
            source: "test".to_string(),
            products: vec!["EMERALDS".to_string()],
            metadata: IndexMap::new(),
            ticks: vec![
                TickSnapshot {
                    timestamp: 0,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![OrderBookLevel {
                                price: 9999,
                                volume: 1,
                            }],
                            asks: vec![OrderBookLevel {
                                price: 10000,
                                volume: 1,
                            }],
                            mid_price: Some(9999.5),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
                TickSnapshot {
                    timestamp: 100,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![],
                            asks: vec![],
                            mid_price: Some(0.0),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
            ],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("empty-book-mark-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: true,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");
        let second_tick = &timeline[1];

        assert_eq!(
            second_tick["products"]["EMERALDS"]["mid_price"].as_f64(),
            Some(9999.5)
        );
        assert_eq!(
            second_tick["pnl_by_product"]["EMERALDS"].as_f64(),
            Some(-0.5)
        );

        let submission_log: Value = serde_json::from_slice(&artifacts.submission_log)
            .expect("submission log JSON should parse");
        let activity_line = submission_log["activitiesLog"]
            .as_str()
            .expect("activitiesLog should be a string")
            .lines()
            .find(|line| line.starts_with("-1;100;EMERALDS;"))
            .expect("activity row for the empty-book tick should exist");
        let fields: Vec<&str> = activity_line.split(';').collect();
        assert_eq!(fields[15], "9999.5");
        assert_eq!(fields[16], "-0.5");

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn one_sided_book_carries_forward_last_stable_mid() {
        let unique = format!(
            "runner-one-sided-mark-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("one_sided_mark_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        if state.timestamp == 0:
            return {"EMERALDS": [Order("EMERALDS", 10000, 1)]}, 0, ""
        return {}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "one-sided-mark-test".to_string(),
            source: "test".to_string(),
            products: vec!["EMERALDS".to_string()],
            metadata: IndexMap::new(),
            ticks: vec![
                TickSnapshot {
                    timestamp: 0,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![OrderBookLevel {
                                price: 9999,
                                volume: 1,
                            }],
                            asks: vec![OrderBookLevel {
                                price: 10000,
                                volume: 1,
                            }],
                            mid_price: Some(9999.5),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
                TickSnapshot {
                    timestamp: 100,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "EMERALDS".to_string(),
                        ProductSnapshot {
                            product: "EMERALDS".to_string(),
                            bids: vec![OrderBookLevel {
                                price: 10020,
                                volume: 1,
                            }],
                            asks: vec![],
                            mid_price: Some(0.0),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
            ],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("one-sided-mark-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: false,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");
        let second_tick = &timeline[1];

        assert_eq!(
            second_tick["products"]["EMERALDS"]["mid_price"].as_f64(),
            Some(9999.5)
        );
        assert_eq!(
            second_tick["pnl_by_product"]["EMERALDS"].as_f64(),
            Some(-0.5)
        );

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn first_one_sided_tick_bootstraps_mark_from_visible_side() {
        let unique = format!(
            "runner-first-one-sided-mark-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("first_one_sided_mark_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        return {"EMERALDS": [Order("EMERALDS", 10000, 1)]}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "first-one-sided-mark-test".to_string(),
            source: "test".to_string(),
            products: vec!["EMERALDS".to_string()],
            metadata: IndexMap::new(),
            ticks: vec![TickSnapshot {
                timestamp: 0,
                day: Some(-1),
                products: IndexMap::from([(
                    "EMERALDS".to_string(),
                    ProductSnapshot {
                        product: "EMERALDS".to_string(),
                        bids: vec![OrderBookLevel {
                            price: 10020,
                            volume: 1,
                        }],
                        asks: vec![],
                        mid_price: Some(0.0),
                    },
                )]),
                market_trades: IndexMap::new(),
                observations: ObservationState::default(),
            }],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("first-one-sided-mark-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: false,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");
        let first_tick = &timeline[0];

        assert_eq!(
            first_tick["products"]["EMERALDS"]["mid_price"].as_f64(),
            Some(10020.0)
        );

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn consumed_market_trade_is_removed_from_bundle_timeline() {
        let unique = format!(
            "runner-market-consume-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("consume_market_trade_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        return {"TOMATOES": [Order("TOMATOES", 4987, 4)]}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "market-consume-test".to_string(),
            source: "test".to_string(),
            products: vec!["TOMATOES".to_string()],
            metadata: IndexMap::new(),
            ticks: vec![TickSnapshot {
                timestamp: 132100,
                day: Some(-1),
                products: IndexMap::from([(
                    "TOMATOES".to_string(),
                    ProductSnapshot {
                        product: "TOMATOES".to_string(),
                        bids: vec![
                            OrderBookLevel {
                                price: 4986,
                                volume: 10,
                            },
                            OrderBookLevel {
                                price: 4985,
                                volume: 19,
                            },
                        ],
                        asks: vec![
                            OrderBookLevel {
                                price: 5000,
                                volume: 10,
                            },
                            OrderBookLevel {
                                price: 5001,
                                volume: 19,
                            },
                        ],
                        mid_price: Some(4993.0),
                    },
                )]),
                market_trades: IndexMap::from([(
                    "TOMATOES".to_string(),
                    vec![crate::model::MarketTrade {
                        symbol: "TOMATOES".to_string(),
                        price: 4986,
                        quantity: 4,
                        buyer: String::new(),
                        seller: String::new(),
                        timestamp: 132100,
                    }],
                )]),
                observations: ObservationState::default(),
            }],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("consume-market-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: true,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");
        let tick = timeline.first().expect("timeline should include one tick");

        assert_eq!(
            tick["own_trades"]
                .as_array()
                .expect("own_trades should be an array")
                .len(),
            1
        );
        assert_eq!(
            tick["market_trades"]
                .as_array()
                .expect("market_trades should be an array")
                .len(),
            0
        );

        let submission_log: Value = serde_json::from_slice(&artifacts.submission_log)
            .expect("submission log JSON should parse");
        let trade_history = submission_log["tradeHistory"]
            .as_array()
            .expect("tradeHistory should be an array");
        assert_eq!(trade_history.len(), 1);
        assert_eq!(trade_history[0]["buyer"].as_str(), Some("SUBMISSION"));
        assert_eq!(trade_history[0]["price"].as_i64(), Some(4987));

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn raw_csv_submission_crosses_book_then_resting_order_is_hit_by_tape() {
        let unique = format!(
            "runner-raw-csv-book-then-tape-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("raw_csv_book_then_tape_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        return {"TOMATOES": [Order("TOMATOES", 10005, 5)]}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "raw-csv-book-then-tape".to_string(),
            source: "test".to_string(),
            products: vec!["TOMATOES".to_string()],
            metadata: IndexMap::from([(
                "source_format".to_string(),
                Value::String("imc_csv".to_string()),
            )]),
            ticks: vec![TickSnapshot {
                timestamp: 0,
                day: Some(-1),
                products: IndexMap::from([(
                    "TOMATOES".to_string(),
                    ProductSnapshot {
                        product: "TOMATOES".to_string(),
                        bids: vec![],
                        asks: vec![OrderBookLevel {
                            price: 10000,
                            volume: 3,
                        }],
                        mid_price: Some(10000.0),
                    },
                )]),
                market_trades: IndexMap::from([(
                    "TOMATOES".to_string(),
                    vec![MarketTrade {
                        symbol: "TOMATOES".to_string(),
                        price: 9990,
                        quantity: 5,
                        buyer: "BOT_BUYER".to_string(),
                        seller: "BOT_SELLER".to_string(),
                        timestamp: 0,
                    }],
                )]),
                observations: ObservationState::default(),
            }],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("raw-csv-book-then-tape-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: true,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");
        let tick = timeline.first().expect("timeline should include one tick");

        let own_trades = tick["own_trades"]
            .as_array()
            .expect("own_trades should be an array");
        assert_eq!(own_trades.len(), 2);
        assert_eq!(own_trades[0]["price"].as_i64(), Some(10000));
        assert_eq!(own_trades[0]["quantity"].as_i64(), Some(3));
        assert_eq!(own_trades[1]["price"].as_i64(), Some(10005));
        assert_eq!(own_trades[1]["quantity"].as_i64(), Some(2));

        let public_trades = tick["market_trades"]
            .as_array()
            .expect("market_trades should be an array");
        assert_eq!(public_trades.len(), 1);
        assert_eq!(public_trades[0]["price"].as_i64(), Some(9990));
        assert_eq!(public_trades[0]["quantity"].as_i64(), Some(3));

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn raw_csv_residual_public_tape_is_carried_to_next_tick_and_logged() {
        let unique = format!(
            "runner-raw-csv-residual-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let output_root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&output_root).expect("temp output root should exist");
        let trader_file = output_root.join("raw_csv_residual_trader.py");
        fs::write(
            &trader_file,
            r#"from datamodel import Order, TradingState

class Trader:
    def run(self, state: TradingState):
        total_prev = sum(trade.quantity for trades in state.market_trades.values() for trade in trades)
        print(f"prev={total_prev}")
        if state.timestamp == 0:
            return {"TOMATOES": [Order("TOMATOES", 10005, 5)]}, 0, ""
        return {}, 0, ""
"#,
        )
        .expect("temp trader file should be written");

        let dataset_override = NormalizedDataset {
            schema_version: "test".to_string(),
            competition_version: "test".to_string(),
            dataset_id: "raw-csv-residual".to_string(),
            source: "test".to_string(),
            products: vec!["TOMATOES".to_string()],
            metadata: IndexMap::from([(
                "source_format".to_string(),
                Value::String("imc_csv".to_string()),
            )]),
            ticks: vec![
                TickSnapshot {
                    timestamp: 0,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "TOMATOES".to_string(),
                        ProductSnapshot {
                            product: "TOMATOES".to_string(),
                            bids: vec![],
                            asks: vec![],
                            mid_price: Some(10000.0),
                        },
                    )]),
                    market_trades: IndexMap::from([(
                        "TOMATOES".to_string(),
                        vec![MarketTrade {
                            symbol: "TOMATOES".to_string(),
                            price: 10000,
                            quantity: 10,
                            buyer: "BOT_BUYER".to_string(),
                            seller: "BOT_SELLER".to_string(),
                            timestamp: 0,
                        }],
                    )]),
                    observations: ObservationState::default(),
                },
                TickSnapshot {
                    timestamp: 100,
                    day: Some(-1),
                    products: IndexMap::from([(
                        "TOMATOES".to_string(),
                        ProductSnapshot {
                            product: "TOMATOES".to_string(),
                            bids: vec![],
                            asks: vec![],
                            mid_price: Some(10000.0),
                        },
                    )]),
                    market_trades: IndexMap::new(),
                    observations: ObservationState::default(),
                },
            ],
        };

        let request = RunRequest {
            trader_file,
            dataset_file: project_root().join("datasets/tutorial/prices_round_0_day_-1.csv"),
            dataset_override: Some(dataset_override),
            day: None,
            matching: MatchingConfig::default(),
            run_id: Some("raw-csv-residual-check".to_string()),
            output_root: output_root.clone(),
            persist: false,
            write_metrics: true,
            write_bundle: true,
            write_submission_log: true,
            materialize_artifacts: true,
            metadata_overrides: Default::default(),
        };

        let output = run_backtest(&request).expect("backtest should succeed");
        let artifacts = output
            .artifacts
            .as_ref()
            .expect("artifacts should be available for materialized runs");
        let bundle: Value =
            serde_json::from_slice(&artifacts.bundle_json).expect("bundle JSON should parse");
        let timeline = bundle["timeline"]
            .as_array()
            .expect("timeline should be an array");

        assert_eq!(
            timeline[0]["market_trades"]
                .as_array()
                .expect("market_trades should be an array")[0]["quantity"]
                .as_i64(),
            Some(5)
        );
        assert_eq!(timeline[1]["algorithm_logs"].as_str(), Some("prev=5"));

        let submission_log: Value = serde_json::from_slice(&artifacts.submission_log)
            .expect("submission log JSON should parse");
        let trade_history = submission_log["tradeHistory"]
            .as_array()
            .expect("tradeHistory should be an array");
        assert_eq!(trade_history.len(), 2);
        assert_eq!(trade_history[0]["price"].as_i64(), Some(10000));
        assert_eq!(trade_history[0]["quantity"].as_i64(), Some(5));
        assert_eq!(trade_history[1]["price"].as_i64(), Some(10005));
        assert_eq!(trade_history[1]["quantity"].as_i64(), Some(5));

        fs::remove_dir_all(output_root).expect("temp output root should be cleaned up");
    }

    #[test]
    fn python_round_helpers_work() {
        assert_eq!(python_round_to_i64(2.5), 2);
        assert_eq!(python_round_to_i64(3.5), 4);
        assert_eq!(python_round_to_digits(1.23456789, 6), 1.234568);
    }

    #[test]
    fn display_path_prefers_project_relative_paths() {
        let path = project_root().join("datasets/tutorial/prices_round_0_day_-1.csv");
        assert_eq!(
            display_path(&path),
            "datasets/tutorial/prices_round_0_day_-1.csv"
        );
    }
}
