use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
struct SubmissionTradeHistoryRow {
    day: Option<i64>,
    trade: MarketTrade,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedDataset {
    pub schema_version: String,
    pub competition_version: String,
    pub dataset_id: String,
    pub source: String,
    pub products: Vec<String>,
    #[serde(default)]
    pub metadata: IndexMap<String, Value>,
    pub ticks: Vec<TickSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickSnapshot {
    pub timestamp: i64,
    pub day: Option<i64>,
    pub products: IndexMap<String, ProductSnapshot>,
    #[serde(default)]
    pub market_trades: IndexMap<String, Vec<MarketTrade>>,
    #[serde(default)]
    pub observations: ObservationState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductSnapshot {
    pub product: String,
    #[serde(default)]
    pub bids: Vec<OrderBookLevel>,
    #[serde(default)]
    pub asks: Vec<OrderBookLevel>,
    pub mid_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookLevel {
    pub price: i64,
    pub volume: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketTrade {
    pub symbol: String,
    pub price: i64,
    pub quantity: i64,
    #[serde(default)]
    pub buyer: String,
    #[serde(default)]
    pub seller: String,
    #[serde(default)]
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObservationState {
    #[serde(default)]
    pub plain: IndexMap<String, i64>,
    #[serde(default)]
    pub conversion: IndexMap<String, IndexMap<String, f64>>,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub symbol: String,
    pub price: i64,
    pub quantity: i64,
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub symbol: String,
    pub price: i64,
    pub quantity: i64,
    pub buyer: String,
    pub seller: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchingConfig {
    pub trade_match_mode: String,
    pub queue_penetration: f64,
    pub price_slippage_bps: f64,
    pub raw_csv_market_trades: String,
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self {
            trade_match_mode: "all".to_string(),
            queue_penetration: 1.0,
            price_slippage_bps: 0.0,
            raw_csv_market_trades: "residual".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MetadataOverrides {
    pub run_id: Option<String>,
    pub generated_at: Option<String>,
    pub recorded_trader_path: Option<String>,
    pub recorded_dataset_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub trader_file: PathBuf,
    pub dataset_file: PathBuf,
    pub dataset_override: Option<NormalizedDataset>,
    pub day: Option<i64>,
    pub matching: MatchingConfig,
    pub run_id: Option<String>,
    pub output_root: PathBuf,
    pub persist: bool,
    pub write_metrics: bool,
    pub write_bundle: bool,
    pub write_submission_log: bool,
    pub materialize_artifacts: bool,
    pub metadata_overrides: MetadataOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetrics {
    pub run_id: String,
    pub dataset_id: String,
    pub dataset_path: String,
    pub trader_path: String,
    pub day: Option<i64>,
    pub matching: MatchingConfig,
    pub tick_count: usize,
    pub own_trade_count: usize,
    pub final_pnl_total: f64,
    pub final_pnl_by_product: IndexMap<String, f64>,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactSet {
    pub metrics_json: Vec<u8>,
    pub bundle_json: Vec<u8>,
    pub submission_log: Vec<u8>,
    pub activity_csv: Vec<u8>,
    pub pnl_by_product_csv: Vec<u8>,
    pub combined_log: Vec<u8>,
    pub trades_csv: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub metrics: RunMetrics,
    pub result_json: Vec<u8>,
    pub artifacts: Option<ArtifactSet>,
}

impl MatchingConfig {
    pub fn mode_is_none(&self) -> bool {
        self.trade_match_mode == "none"
    }
}

pub fn load_dataset(path: &Path) -> Result<NormalizedDataset> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => load_price_csv_dataset(path),
        "log" => load_submission_log_dataset(path),
        "json" => load_json_dataset(path),
        _ => bail!(
            "unsupported dataset format for {}; expected JSON, prices CSV, or submission log",
            path.display()
        ),
    }
}

pub fn materialize_submission_json_if_missing(path: &Path) -> Result<Option<PathBuf>> {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        != "log"
    {
        return Ok(None);
    }

    let json_path = path.with_extension("json");
    if json_path.is_file() {
        return Ok(None);
    }

    let Some(value) = read_submission_log_payload(path)? else {
        return Ok(None);
    };
    let dataset = load_submission_value_dataset(path, &value)?;
    let payload = serde_json::to_vec_pretty(&dataset).with_context(|| {
        format!(
            "failed to serialize normalized submission dataset for {}",
            path.display()
        )
    })?;
    fs::write(&json_path, payload).with_context(|| {
        format!(
            "failed to write normalized submission dataset {}",
            json_path.display()
        )
    })?;
    Ok(Some(json_path))
}

const ACTIVITY_HEADER_PREFIX: &str =
    "day;timestamp;product;bid_price_1;bid_volume_1;bid_price_2;bid_volume_2";
const TRADE_HEADER_PREFIX: &str = "timestamp;buyer;seller;symbol;currency;price;quantity";
const OBSERVATION_HEADER_PREFIX: &str =
    "timestamp,bidPrice,askPrice,transportFees,exportTariff,importTariff,sugarPrice,sunlightIndex";
const ROUND4_CONVERSION_PRODUCT: &str = "MAGNIFICENT_MACARONS";

fn load_json_dataset(path: &Path) -> Result<NormalizedDataset> {
    let payload = fs::read_to_string(path)
        .with_context(|| format!("failed to read dataset file {}", path.display()))?;
    if let Ok(dataset) = serde_json::from_str::<NormalizedDataset>(&payload) {
        return Ok(dataset);
    }

    let value: Value = serde_json::from_str(&payload)
        .with_context(|| format!("failed to parse dataset JSON {}", path.display()))?;
    if value.get("activitiesLog").and_then(Value::as_str).is_some() {
        return load_submission_value_dataset(path, &value);
    }

    bail!("failed to parse supported dataset JSON {}", path.display())
}

fn load_submission_log_dataset(path: &Path) -> Result<NormalizedDataset> {
    let value = read_submission_log_payload(path)?
        .with_context(|| format!("failed to parse submission log JSON {}", path.display()))?;
    load_submission_value_dataset(path, &value)
}

fn read_submission_log_payload(path: &Path) -> Result<Option<Value>> {
    let payload = fs::read_to_string(path)
        .with_context(|| format!("failed to read log {}", path.display()))?;
    let Ok(value) = serde_json::from_str::<Value>(&payload) else {
        return Ok(None);
    };
    if value.get("activitiesLog").and_then(Value::as_str).is_none() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn load_submission_value_dataset(path: &Path, value: &Value) -> Result<NormalizedDataset> {
    let activities_log = value
        .get("activitiesLog")
        .and_then(Value::as_str)
        .with_context(|| {
            format!(
                "submission payload missing activitiesLog in {}",
                path.display()
            )
        })?;
    let trade_history = value
        .get("tradeHistory")
        .and_then(Value::as_array)
        .map(|rows| parse_submission_trade_history(rows))
        .transpose()?
        .unwrap_or_default();

    let mut metadata = IndexMap::new();
    metadata.insert("built_from".to_string(), Value::String(path_string(path)));

    build_dataset_from_activities(
        path,
        submission_dataset_id_from_path(path),
        path_string(path),
        activities_log,
        trade_history,
        metadata,
    )
}

fn load_price_csv_dataset(path: &Path) -> Result<NormalizedDataset> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !file_name.starts_with("prices_") {
        bail!(
            "unsupported CSV input {}; pass a prices_*.csv file or a directory containing IMC CSV files",
            path.display()
        );
    }

    let activities_log = fs::read_to_string(path)
        .with_context(|| format!("failed to read prices CSV {}", path.display()))?;
    let trade_path = paired_trades_csv(path).with_context(|| {
        format!(
            "unsupported CSV input {}; expected a prices_*.csv filename",
            path.display()
        )
    })?;
    if !trade_path.is_file() {
        bail!(
            "missing paired trades CSV for {}; expected {}",
            path.display(),
            trade_path.display()
        );
    }
    let trade_history = load_trades_csv(&trade_path)?
        .into_iter()
        .map(|trade| SubmissionTradeHistoryRow { day: None, trade })
        .collect::<Vec<_>>();
    let observation_path = paired_observations_csv(path).with_context(|| {
        format!(
            "unsupported CSV input {}; expected a prices_*.csv filename",
            path.display()
        )
    })?;
    let day_hint = day_from_path(path);
    let mut observations_by_key = if observation_path.is_file() {
        load_observations_csv(&observation_path, day_hint)?
    } else {
        BTreeMap::new()
    };

    let mut metadata = IndexMap::new();
    metadata.insert(
        "source_format".to_string(),
        Value::String("imc_csv".to_string()),
    );
    metadata.insert(
        "trade_rows".to_string(),
        Value::Number((trade_history.len() as u64).into()),
    );
    metadata.insert(
        "observation_rows".to_string(),
        Value::Number((observations_by_key.len() as u64).into()),
    );

    let mut dataset = build_dataset_from_activities(
        path,
        dataset_id_from_path(path),
        format!("imc_csv:{}", file_name),
        &activities_log,
        trade_history,
        metadata,
    )?;

    for tick in &mut dataset.ticks {
        if let Some(observations) = observations_by_key
            .remove(&(tick.day, tick.timestamp))
            .or_else(|| observations_by_key.remove(&(None, tick.timestamp)))
        {
            tick.observations = observations;
        }
    }

    Ok(dataset)
}

fn build_dataset_from_activities(
    path: &Path,
    dataset_id: String,
    source: String,
    activities_log: &str,
    trade_history: Vec<SubmissionTradeHistoryRow>,
    metadata: IndexMap<String, Value>,
) -> Result<NormalizedDataset> {
    let mut products_seen: IndexMap<String, ()> = IndexMap::new();
    let mut ticks_by_key: BTreeMap<(Option<i64>, i64), TickSnapshot> = BTreeMap::new();
    let mut activity_row_count = 0u64;
    let trade_row_count = trade_history.len() as u64;

    for (line_number, line) in activities_log.lines().enumerate() {
        if line_number == 0 {
            if !line.starts_with(ACTIVITY_HEADER_PREFIX) {
                bail!("unexpected activities header in {}", path.display());
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() < 17 {
            bail!(
                "invalid activities row {} in {}; expected at least 17 columns",
                line_number + 1,
                path.display()
            );
        }

        let day = parse_optional_i64(fields[0])?;
        let timestamp = parse_required_i64(fields[1], "timestamp")?;
        let product = fields[2].trim();
        if product.is_empty() {
            bail!(
                "missing product in activities row {} of {}",
                line_number + 1,
                path.display()
            );
        }

        let snapshot = ProductSnapshot {
            product: product.to_string(),
            bids: parse_book_side(&fields, &[(3, 4), (5, 6), (7, 8)])?,
            asks: parse_book_side(&fields, &[(9, 10), (11, 12), (13, 14)])?,
            mid_price: parse_optional_f64(fields[15])?,
        };

        activity_row_count += 1;
        products_seen.entry(product.to_string()).or_insert(());
        ticks_by_key
            .entry((day, timestamp))
            .or_insert_with(|| TickSnapshot {
                timestamp,
                day,
                products: IndexMap::new(),
                market_trades: IndexMap::new(),
                observations: ObservationState::default(),
            })
            .products
            .insert(product.to_string(), snapshot);
    }

    if ticks_by_key.is_empty() {
        bail!("no tick rows found in {}", path.display());
    }

    let mut trades_by_key: BTreeMap<(Option<i64>, i64), IndexMap<String, Vec<MarketTrade>>> =
        BTreeMap::new();
    for trade in trade_history {
        trades_by_key
            .entry((trade.day, trade.trade.timestamp))
            .or_default()
            .entry(trade.trade.symbol.clone())
            .or_default()
            .push(trade.trade);
    }

    let mut ticks: Vec<TickSnapshot> = ticks_by_key.into_values().collect();
    for tick in &mut ticks {
        if let Some(market_trades) = trades_by_key
            .remove(&(tick.day, tick.timestamp))
            .or_else(|| trades_by_key.remove(&(None, tick.timestamp)))
        {
            tick.market_trades = market_trades;
        }
    }

    let mut products: Vec<String> = products_seen.into_keys().collect();
    products.sort();

    let mut full_metadata = IndexMap::new();
    full_metadata.insert(
        "activity_rows".to_string(),
        Value::Number(activity_row_count.into()),
    );
    full_metadata.extend(metadata);
    if !full_metadata.contains_key("trade_rows") {
        full_metadata.insert(
            "trade_rows".to_string(),
            Value::Number(trade_row_count.into()),
        );
    }

    Ok(NormalizedDataset {
        schema_version: "1.0".to_string(),
        competition_version: "p4".to_string(),
        dataset_id,
        source,
        products,
        metadata: full_metadata,
        ticks,
    })
}

fn parse_book_side(fields: &[&str], pairs: &[(usize, usize)]) -> Result<Vec<OrderBookLevel>> {
    let mut levels = Vec::new();
    for &(price_index, volume_index) in pairs {
        let Some(price_text) = fields.get(price_index).copied() else {
            continue;
        };
        let Some(volume_text) = fields.get(volume_index).copied() else {
            continue;
        };
        if price_text.trim().is_empty() || volume_text.trim().is_empty() {
            continue;
        }
        levels.push(OrderBookLevel {
            price: parse_price_i64(price_text)?,
            volume: parse_required_i64(volume_text, "volume")?,
        });
    }
    Ok(levels)
}

fn parse_submission_trade_history(rows: &[Value]) -> Result<Vec<SubmissionTradeHistoryRow>> {
    rows.iter()
        .map(|row| {
            let object = row
                .as_object()
                .context("tradeHistory rows should be JSON objects")?;
            Ok(SubmissionTradeHistoryRow {
                day: object.get("day").and_then(Value::as_i64),
                trade: MarketTrade {
                    symbol: object
                        .get("symbol")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    price: parse_trade_value_price(object.get("price"))?,
                    quantity: object
                        .get("quantity")
                        .and_then(Value::as_i64)
                        .context("tradeHistory row missing quantity")?,
                    buyer: object
                        .get("buyer")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    seller: object
                        .get("seller")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    timestamp: object
                        .get("timestamp")
                        .and_then(Value::as_i64)
                        .context("tradeHistory row missing timestamp")?,
                },
            })
        })
        .collect()
}

fn load_trades_csv(path: &Path) -> Result<Vec<MarketTrade>> {
    let payload = fs::read_to_string(path)
        .with_context(|| format!("failed to read trades CSV {}", path.display()))?;
    let mut trades = Vec::new();

    for (line_number, line) in payload.lines().enumerate() {
        if line_number == 0 {
            if !line.starts_with(TRADE_HEADER_PREFIX) {
                bail!("unexpected trades header in {}", path.display());
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() < 7 {
            bail!(
                "invalid trades row {} in {}; expected 7 columns",
                line_number + 1,
                path.display()
            );
        }

        trades.push(MarketTrade {
            timestamp: parse_required_i64(fields[0], "timestamp")?,
            buyer: fields[1].trim().to_string(),
            seller: fields[2].trim().to_string(),
            symbol: fields[3].trim().to_string(),
            price: parse_price_i64(fields[5])?,
            quantity: parse_required_i64(fields[6], "quantity")?,
        });
    }

    Ok(trades)
}

fn load_observations_csv(
    path: &Path,
    day_hint: Option<i64>,
) -> Result<BTreeMap<(Option<i64>, i64), ObservationState>> {
    let payload = fs::read_to_string(path)
        .with_context(|| format!("failed to read observations CSV {}", path.display()))?;
    let mut observations = BTreeMap::new();

    for (line_number, line) in payload.lines().enumerate() {
        if line_number == 0 {
            if line.trim() != OBSERVATION_HEADER_PREFIX {
                bail!("unexpected observations header in {}", path.display());
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 8 {
            bail!(
                "invalid observations row {} in {}; expected 8 columns",
                line_number + 1,
                path.display()
            );
        }

        let timestamp = parse_required_i64(fields[0], "timestamp")?;
        let mut conversion = IndexMap::new();
        let mut values = IndexMap::new();
        values.insert(
            "bidPrice".to_string(),
            parse_required_f64(fields[1], "bidPrice")?,
        );
        values.insert(
            "askPrice".to_string(),
            parse_required_f64(fields[2], "askPrice")?,
        );
        values.insert(
            "transportFees".to_string(),
            parse_required_f64(fields[3], "transportFees")?,
        );
        values.insert(
            "exportTariff".to_string(),
            parse_required_f64(fields[4], "exportTariff")?,
        );
        values.insert(
            "importTariff".to_string(),
            parse_required_f64(fields[5], "importTariff")?,
        );
        values.insert(
            "sugarPrice".to_string(),
            parse_required_f64(fields[6], "sugarPrice")?,
        );
        values.insert(
            "sunlightIndex".to_string(),
            parse_required_f64(fields[7], "sunlightIndex")?,
        );
        conversion.insert(ROUND4_CONVERSION_PRODUCT.to_string(), values);

        observations.insert(
            (day_hint, timestamp),
            ObservationState {
                plain: IndexMap::new(),
                conversion,
            },
        );
    }

    Ok(observations)
}

fn parse_optional_i64(value: &str) -> Result<Option<i64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(value.trim().parse::<i64>().with_context(|| {
        format!("failed to parse integer value {value}")
    })?))
}

fn parse_required_i64(value: &str, field_name: &str) -> Result<i64> {
    value
        .trim()
        .parse::<i64>()
        .with_context(|| format!("failed to parse {field_name} value {value}"))
}

fn parse_optional_f64(value: &str) -> Result<Option<f64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(value.trim().parse::<f64>().with_context(|| {
        format!("failed to parse float value {value}")
    })?))
}

fn parse_required_f64(value: &str, field_name: &str) -> Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("failed to parse {field_name} value {value}"))
}

fn parse_price_i64(value: &str) -> Result<i64> {
    Ok(value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("failed to parse price value {value}"))?
        .round() as i64)
}

fn parse_trade_value_price(value: Option<&Value>) -> Result<i64> {
    let Some(value) = value else {
        bail!("tradeHistory row missing price");
    };
    if let Some(integer) = value.as_i64() {
        return Ok(integer);
    }
    if let Some(number) = value.as_f64() {
        return Ok(number.round() as i64);
    }
    if let Some(text) = value.as_str() {
        return parse_price_i64(text);
    }
    bail!("tradeHistory row has unsupported price value");
}

fn paired_trades_csv(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    if !file_name.starts_with("prices_") {
        return None;
    }
    Some(path.with_file_name(file_name.replacen("prices_", "trades_", 1)))
}

fn paired_observations_csv(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    if !file_name.starts_with("prices_") {
        return None;
    }
    Some(path.with_file_name(file_name.replacen("prices_", "observations_", 1)))
}

fn day_from_path(path: &Path) -> Option<i64> {
    let file_name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let start = file_name.find("day_")?;
    let suffix = &file_name[start + 4..];
    let end = suffix
        .find(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .unwrap_or(suffix.len());
    suffix[..end].parse::<i64>().ok()
}

fn dataset_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("dataset")
        .to_string()
}

fn submission_dataset_id_from_path(path: &Path) -> String {
    let stem = dataset_id_from_path(path);
    if !stem.is_empty() && stem.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("official_submission_{stem}_alltrades");
    }
    stem
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{load_dataset, load_submission_value_dataset};
    use serde_json::json;
    use std::{fs, path::Path};

    #[test]
    fn submission_trade_history_uses_day_when_present() {
        let payload = json!({
            "activitiesLog": concat!(
                "day;timestamp;product;bid_price_1;bid_volume_1;bid_price_2;bid_volume_2;bid_price_3;bid_volume_3;ask_price_1;ask_volume_1;ask_price_2;ask_volume_2;ask_price_3;ask_volume_3;mid_price;profit_and_loss\n",
                "-2;0;EMERALDS;9992;15;9990;30;;;10008;15;10010;30;;;10000.0;0.0\n",
                "-1;0;EMERALDS;9992;15;9990;30;;;10008;15;10010;30;;;10000.0;0.0"
            ),
            "tradeHistory": [
                {
                    "day": -2,
                    "timestamp": 0,
                    "buyer": "A",
                    "seller": "B",
                    "symbol": "EMERALDS",
                    "currency": "SEASHELLS",
                    "price": 10000,
                    "quantity": 1
                },
                {
                    "day": -1,
                    "timestamp": 0,
                    "buyer": "C",
                    "seller": "D",
                    "symbol": "EMERALDS",
                    "currency": "SEASHELLS",
                    "price": 10001,
                    "quantity": 2
                }
            ]
        });

        let dataset = load_submission_value_dataset(Path::new("combined.log"), &payload)
            .expect("submission payload should load");

        assert_eq!(dataset.ticks.len(), 2);
        assert_eq!(dataset.ticks[0].day, Some(-2));
        assert_eq!(dataset.ticks[1].day, Some(-1));
        assert_eq!(dataset.ticks[0].market_trades["EMERALDS"][0].quantity, 1);
        assert_eq!(dataset.ticks[1].market_trades["EMERALDS"][0].quantity, 2);
    }

    #[test]
    fn missing_paired_trades_csv_is_an_error() {
        let unique = format!(
            "model-missing-trades-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let scratch = std::env::temp_dir().join(unique);
        fs::create_dir_all(&scratch).expect("scratch dir should exist");

        let prices_path = scratch.join("prices_round_1_day_0.csv");
        fs::write(
            &prices_path,
            concat!(
                "day;timestamp;product;bid_price_1;bid_volume_1;bid_price_2;bid_volume_2;bid_price_3;bid_volume_3;",
                "ask_price_1;ask_volume_1;ask_price_2;ask_volume_2;ask_price_3;ask_volume_3;mid_price;profit_and_loss\n",
                "0;0;EMERALDS;9999;5;;;;;10001;5;;;;;10000.0;0.0\n"
            ),
        )
        .expect("prices csv should be written");

        let error = load_dataset(&prices_path).expect_err("missing paired trades should fail");
        let message = format!("{error:#}");
        assert!(message.contains("missing paired trades CSV"));
        assert!(message.contains("trades_round_1_day_0.csv"));

        fs::remove_dir_all(scratch).expect("scratch dir should be cleaned up");
    }

    #[test]
    fn paired_observations_csv_populates_conversion_observations() {
        let unique = format!(
            "model-observations-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        );
        let scratch = std::env::temp_dir().join(unique);
        fs::create_dir_all(&scratch).expect("scratch dir should exist");

        let prices_path = scratch.join("prices_round_4_day_1.csv");
        let trades_path = scratch.join("trades_round_4_day_1.csv");
        let observations_path = scratch.join("observations_round_4_day_1.csv");

        fs::write(
            &prices_path,
            concat!(
                "day;timestamp;product;bid_price_1;bid_volume_1;bid_price_2;bid_volume_2;bid_price_3;bid_volume_3;",
                "ask_price_1;ask_volume_1;ask_price_2;ask_volume_2;ask_price_3;ask_volume_3;mid_price;profit_and_loss\n",
                "1;0;MAGNIFICENT_MACARONS;624;20;;;;;631;10;;;;;627.5;0.0\n"
            ),
        )
        .expect("prices csv should be written");
        fs::write(
            &trades_path,
            concat!(
                "timestamp;buyer;seller;symbol;currency;price;quantity\n",
                "0;;;MAGNIFICENT_MACARONS;SEASHELLS;631.0;1\n"
            ),
        )
        .expect("trades csv should be written");
        fs::write(
            &observations_path,
            concat!(
                "timestamp,bidPrice,askPrice,transportFees,exportTariff,importTariff,sugarPrice,sunlightIndex\n",
                "0,627.0,628.5,1.0,9.0,-3.0,200.0,60.0\n"
            ),
        )
        .expect("observations csv should be written");

        let dataset = load_dataset(&prices_path).expect("round 4 csv dataset should load");
        let observations = &dataset.ticks[0].observations.conversion["MAGNIFICENT_MACARONS"];

        assert_eq!(dataset.ticks[0].day, Some(1));
        assert_eq!(observations["bidPrice"], 627.0);
        assert_eq!(observations["askPrice"], 628.5);
        assert_eq!(observations["transportFees"], 1.0);
        assert_eq!(observations["sunlightIndex"], 60.0);

        fs::remove_dir_all(scratch).expect("scratch dir should be cleaned up");
    }
}
