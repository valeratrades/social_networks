use color_eyre::eyre::Result;
use v_exchanges::ExchangeName;

pub(crate) async fn btc_price(n_retries: u8) -> Result<u64> {
	let mut binance_exchange = ExchangeName::Binance.init_client();
	binance_exchange.set_max_tries(n_retries);

	let price = binance_exchange.price("BTC-USDT.P".into()).await?;
	Ok(price as u64)
}

pub(crate) fn format_num_with_thousands(num: u64, sep: &'static str) -> String {
	num.to_string()
		.as_bytes()
		.rchunks(3)
		.rev()
		.map(std::str::from_utf8)
		.collect::<Result<Vec<&str>, _>>()
		.unwrap()
		.join(sep)
}
