use social_networks_adapters::behaviour::Order;

#[test]
fn an_order_is_a_banded_shuffle_that_survives_a_restart() {
	let dir = std::env::temp_dir().join(format!("order_{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let seed = dir.join("seed");
	let first = Order::load(0..137, &seed).unwrap();
	assert_eq!(*Order::load(0..137, &seed).unwrap(), *first);
	assert_ne!(*first, (0..137).collect::<Vec<_>>()[..], "shuffled");

	let mut sorted = first.to_vec();
	sorted.sort();
	assert_eq!(sorted, (0..137).collect::<Vec<_>>());
	for (k, band) in first.chunks(50).enumerate() {
		assert!(band.iter().all(|i| i / 50 == k), "band {k} holds {band:?}");
	}
	std::fs::remove_dir_all(&dir).unwrap();
}
