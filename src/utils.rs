use color_eyre::eyre::Result;
use v_exchanges::ExchangeName;

/// Returns (stack_used, stack_remaining) in bytes
pub fn stack_usage() -> (usize, usize) {
	let remaining = psm::stack_pointer() as usize;
	// Stack grows downward on most platforms, so we estimate usage from a baseline
	// This is approximate - we measure from a known high point
	thread_local! {
		static STACK_BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
	}

	STACK_BASE.with(|base| {
		if base.get() == 0 {
			base.set(remaining);
		}
		let used = base.get().saturating_sub(remaining);
		(used, remaining)
	})
}

/// Log current stack usage at warn level for debugging stack overflow issues
#[inline(never)]
pub fn log_stack_usage(context: &str) {
	let (used, _remaining) = stack_usage();
	// Log at different levels based on usage
	if used > 2 * 1024 * 1024 {
		tracing::error!("[STACK] {context}: used {:.2}MB", used as f64 / (1024.0 * 1024.0));
	} else if used > 1024 * 1024 {
		tracing::warn!("[STACK] {context}: used {:.2}MB", used as f64 / (1024.0 * 1024.0));
	} else if used > 256 * 1024 {
		tracing::info!("[STACK] {context}: used {:.0}KB", used as f64 / 1024.0);
	} else if used > 64 * 1024 {
		tracing::debug!("[STACK] {context}: used {:.0}KB", used as f64 / 1024.0);
	}
}

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
