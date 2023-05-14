use std::{collections::HashMap, env, rc::Rc};

use anchor_client::Client;
use anyhow::Result;
use fixed::types::I80F48;
use lazy_static::lazy_static;
use marginfi::{
    constants::{EMISSIONS_FLAG_BORROW_ACTIVE, EMISSIONS_FLAG_LENDING_ACTIVE, SECONDS_PER_YEAR},
    state::marginfi_group::Bank,
};
use reqwest::header::CONTENT_TYPE;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{program_pack::Pack, pubkey::Pubkey, signature::Keypair};

lazy_static! {
    static ref TOKEN_LIST: HashMap<&'static str, &'static str> = {
        lazy_static! {
            static ref TOKENS_JSON: String = std::fs::read_to_string("tokens.json").unwrap();
        }
        serde_json::from_str(&TOKENS_JSON).unwrap()
    };
}

const BIRDEYE_API: &str = "https://public-api.birdeye.so";
const CHAIN: &str = "solana";
const PROJECT: &str = "marginfi";

fn main() {
    let dummy_key = Keypair::new();
    let rpc_url = env::var("RPC_ENDPOINT").unwrap();
    let client = Client::new(
        anchor_client::Cluster::Custom(rpc_url.to_string(), rpc_url.to_string()),
        Rc::new(dummy_key),
    );

    let program = client.program(marginfi::id());
    let rpc = program.rpc();

    let banks = program.accounts::<Bank>(vec![]).unwrap();

    println!("Found {} banks", banks.len());

    let snapshot = banks
        .iter()
        .map(|(bank_pk, bank)| DefiLammaPoolInfo::from_bank(bank, bank_pk, &rpc))
        .collect::<Vec<_>>();

    println!("Banks: {:#?}", snapshot);
}

#[derive(Clone, Debug, Default)]
struct DefiLammaPoolInfo {
    pool: String,
    chain: String,
    project: String,
    symbol: String,
    tvl_usd: f64,
    total_supply_usd: f64,
    total_borrow_usd: f64,
    ltv: f64,
    apy_base: f64,
    apy_reward: Option<f64>,
    apy_base_borrow: f64,
    apy_reward_borrow: Option<f64>,
    reward_tokens: Vec<String>,
    underlying_tokens: Vec<String>,
}

impl DefiLammaPoolInfo {
    pub fn from_bank(bank: &Bank, bank_pk: &Pubkey, rpc_client: &RpcClient) -> Self {
        let ltv = I80F48::ONE / I80F48::from(bank.config.liability_weight_init);
        let reward_tokens = if bank.emissions_mint != Pubkey::default() {
            vec![bank.emissions_mint.to_string()]
        } else {
            vec![]
        };

        let token_price = fetch_price_from_birdeye(&bank.mint).unwrap();
        let scale = I80F48::from_num(10_i32.pow(bank.mint_decimals as u32));

        let total_deposits = bank
            .get_asset_amount(bank.total_asset_shares.into())
            .unwrap()
            / scale;
        let total_borrows = bank
            .get_liability_amount(bank.total_liability_shares.into())
            .unwrap()
            / scale;

        let net_supply = total_deposits - total_borrows;

        let tvl_usd = token_price * net_supply;

        let total_supply_usd = token_price * total_deposits;
        let total_borrow_usd = token_price * total_borrows;

        let token_mint = bank.mint.to_string();

        let ur = if total_deposits > 0 {
            total_borrows / total_deposits
        } else {
            I80F48::ZERO
        };

        let (lending_rate, borrowing_rate, _, _) = bank
            .config
            .interest_rate_config
            .calc_interest_rate(ur)
            .unwrap();

        let (apr_reward, apr_reward_borrow) = if bank.emissions_mint.ne(&Pubkey::default()) {
            let emissions_token_price = fetch_price_from_birdeye(&bank.emissions_mint).unwrap();
            let mint = rpc_client.get_account(&bank.emissions_mint).unwrap();
            let mint = spl_token::state::Mint::unpack_from_slice(&mint.data).unwrap();

            // rate / 10 ^ decimals
            let reward_rate_per_token =
                bank.emissions_rate as f64 / 10i32.pow(mint.decimals as u32) as f64;
            let relative_emissions_value = (emissions_token_price
                * I80F48::from_num(reward_rate_per_token))
                / I80F48::from_num(token_price);

            (
                if bank.get_emissions_flag(EMISSIONS_FLAG_LENDING_ACTIVE) {
                    Some(relative_emissions_value)
                } else {
                    None
                },
                if bank.get_emissions_flag(EMISSIONS_FLAG_BORROW_ACTIVE) {
                    Some(relative_emissions_value)
                } else {
                    None
                },
            )
        } else {
            (None, None)
        };

        Self {
            pool: bank_pk.to_string(),
            chain: CHAIN.to_string(),
            project: PROJECT.to_string(),
            symbol: TOKEN_LIST
                .get(token_mint.as_str())
                .unwrap_or(&"Unknown Token")
                .to_string(),
            tvl_usd: tvl_usd.to_num(),
            total_supply_usd: total_supply_usd.to_num(),
            total_borrow_usd: total_borrow_usd.to_num(),
            ltv: ltv.to_num(),
            reward_tokens,
            apy_base: apr_to_apy(lending_rate.to_num(), SECONDS_PER_YEAR.to_num()),
            apy_reward: apr_reward
                .map(|a| apr_to_apy((lending_rate + a).to_num(), SECONDS_PER_YEAR.to_num())),
            apy_base_borrow: apr_to_apy(borrowing_rate.to_num(), SECONDS_PER_YEAR.to_num()),
            apy_reward_borrow: apr_reward_borrow
                .map(|a| apr_to_apy((borrowing_rate + a).to_num(), SECONDS_PER_YEAR.to_num())),
            underlying_tokens: vec![bank.mint.to_string()],
        }
    }
}
fn fetch_price_from_birdeye(token: &Pubkey) -> Result<I80F48> {
    let url = format!("{}/public/price?address={}", BIRDEYE_API, token.to_string());
    let client = reqwest::blocking::Client::new();

    let res = client
        .get(&url)
        .header(CONTENT_TYPE, "application/json")
        .send()?;

    let body = res.json::<serde_json::Value>()?;

    let price = body
        .as_object()
        .unwrap()
        .get("data")
        .unwrap()
        .get("value")
        .unwrap()
        .as_f64();

    Ok(I80F48::from_num(price.unwrap()))
}

fn apr_to_apy(apr: f64, m: f64) -> f64 {
    (1. + (apr / m)).powf(m) - 1.
}
