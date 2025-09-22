use ethers::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use serde::Deserialize;
use anyhow::{Result, Context};
use log::{info, error};
use sqlx::SqlitePool;
use chrono::Utc;

abigen!(
    UniswapV2Router,
    r#"[
        function getAmountsOut(uint amountIn, address[] memory path) external view returns (uint[] memory amounts)
    ]"#
);

#[derive(Deserialize, Debug)]
struct Config {
    rpc_url: String,
    dex_routers: Vec<DexRouterConfig>,
    tokens: TokenConfig,
    trade_size: f64,
    min_profit_threshold: f64,
    simulated_gas_cost: f64,
}

#[derive(Deserialize, Debug)]
struct DexRouterConfig {
    name: String,
    address: String,
}

#[derive(Deserialize, Debug)]
struct TokenConfig {
    base_token: String,
    quote_token: String,
}

struct PriceQuote {
    dex_name: String,
    price: f64,
}

// Token decimals constants
const BASE_TOKEN_DECIMALS: u32 = 18;  // WETH decimals
const QUOTE_TOKEN_DECIMALS: u32 = 6;  // USDC decimals

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logger
    env_logger::init();

    info!("Current working directory: {:?}", std::env::current_dir().context("Failed to get current directory")?);

    // Load config
    let config = load_config("config.json").context("Failed to load config.json")?;
    info!("Loaded config: {:?}", config);

    // Connect to Polygon RPC
    let provider = Provider::<Http>::try_from(config.rpc_url.as_str())
        .context("Failed to create provider from RPC URL")?;
    let client = Arc::new(provider);

    // Connect to SQLite database
    let db_pool = SqlitePool::connect("sqlite:C:/Deqode_project/polygon-arbitrage-bot/arbitrage.db")
        .await
        .context("Failed to connect to SQLite database")?;

    // Create table if not exists
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS opportunities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT,
            dex_buy TEXT,
            dex_sell TEXT,
            token_pair TEXT,
            buy_price REAL,
            sell_price REAL,
            profit REAL
        )
        "#,
    )
    .execute(&db_pool)
    .await
    .context("Failed to create opportunities table")?;

    // Parse token addresses
    let base_token = config.tokens.base_token.parse::<Address>()
        .context("Failed to parse base_token address")?;
    let quote_token = config.tokens.quote_token.parse::<Address>()
        .context("Failed to parse quote_token address")?;

    // Convert trade size to wei (base token decimals)
    let trade_size_wei = float_to_wei(config.trade_size, BASE_TOKEN_DECIMALS);

    info!("Starting arbitrage bot loop...");

    loop {
        let mut prices = Vec::new();

        for dex in &config.dex_routers {
            let router_addr = dex.address.parse::<Address>()
                .with_context(|| format!("Failed to parse router address for {}", dex.name))?;
            let path = vec![base_token, quote_token];

            match fetch_price(client.clone(), router_addr, trade_size_wei, path, &dex.name).await {
                Ok(price) => {
                    info!("{} price: {:.6}", dex.name, price);
                    prices.push(PriceQuote {
                        dex_name: dex.name.clone(),
                        price,
                    });
                }
                Err(e) => {
                    error!("Error fetching price from {}: {:?}", dex.name, e);
                }
            }
        }

        // Detect arbitrage opportunities
        for buy in &prices {
            for sell in &prices {
                if buy.dex_name == sell.dex_name {
                    continue;
                }

                if let Some(profit) = detect_arbitrage(
                    buy,
                    sell,
                    config.trade_size,
                    config.simulated_gas_cost,
                    config.min_profit_threshold,
                ) {
                    info!(
                        "Arbitrage opportunity detected! Buy on {} at {:.6}, sell on {} at {:.6}, profit: {:.6} USDC",
                        buy.dex_name, buy.price, sell.dex_name, sell.price, profit
                    );

                    if let Err(e) = store_opportunity(
                        &db_pool,
                        &buy.dex_name,
                        &sell.dex_name,
                        &format!("{}/{}", config.tokens.base_token, config.tokens.quote_token),
                        buy.price,
                        sell.price,
                        profit,
                    )
                    .await
                    {
                        error!("Failed to store opportunity in DB: {:?}", e);
                    }
                }
            }
        }

        sleep(Duration::from_secs(30)).await;
    }
}

fn load_config(path: &str) -> Result<Config> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file at path '{}'", path))?;
    let config: Config = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse JSON from config file '{}'", path))?;
    Ok(config)
}

fn float_to_wei(amount: f64, decimals: u32) -> U256 {
    let base = 10u128.pow(decimals);
    let wei = (amount * base as f64) as u128;
    U256::from(wei)
}

fn wei_to_float(amount: U256, decimals: u32) -> f64 {
    let base: f64 = 10f64.powi(decimals as i32);
    let amount_f64 = amount.as_u128() as f64;
    amount_f64 / base
}

async fn fetch_price(
    client: Arc<Provider<Http>>,
    router_address: Address,
    amount_in: U256,
    path: Vec<Address>,
    dex_name: &str,
) -> Result<f64> {
    let router = UniswapV2Router::new(router_address, client);

    // Add timeout to avoid hanging
    let amounts_out = timeout(Duration::from_secs(10), router.get_amounts_out(amount_in, path).call())
        .await
        .context("Timeout calling getAmountsOut")??;

    info!("{} raw amounts_out: {:?}", dex_name, amounts_out);

    if amounts_out.is_empty() {
        error!("getAmountsOut returned empty amounts for {}", dex_name);
        return Ok(0.0);
    }

    let output_amount = amounts_out.last().unwrap();

    // Use quote token decimals here (USDC = 6 decimals)
    let price = wei_to_float(*output_amount, QUOTE_TOKEN_DECIMALS);

    Ok(price)
}

fn detect_arbitrage(
    buy: &PriceQuote,
    sell: &PriceQuote,
    trade_size: f64,
    gas_cost: f64,
    min_profit_threshold: f64,
) -> Option<f64> {
    let profit = (sell.price - buy.price) * trade_size - gas_cost;

    if profit > min_profit_threshold {
        Some(profit)
    } else {
        None
    }
}

async fn store_opportunity(
    pool: &SqlitePool,
    dex_buy: &str,
    dex_sell: &str,
    token_pair: &str,
    buy_price: f64,
    sell_price: f64,
    profit: f64,
) -> Result<()> {
    // Get current UTC time and format as string
    let now = Utc::now().naive_utc();
    let now_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    sqlx::query(
        r#"
        INSERT INTO opportunities (timestamp, dex_buy, dex_sell, token_pair, buy_price, sell_price, profit)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(now_str)       // Bind timestamp as formatted string
    .bind(dex_buy)
    .bind(dex_sell)
    .bind(token_pair)
    .bind(buy_price)
    .bind(sell_price)
    .bind(profit)
    .execute(pool)
    .await
    .context("Failed to insert opportunity into database")?;

    Ok(())
}
