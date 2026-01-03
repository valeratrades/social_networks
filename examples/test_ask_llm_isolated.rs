use color_eyre::eyre::Result;

#[tokio::test]
#[ignore] // Requires API credentials
async fn test_ask_llm_isolated() -> Result<()> {
	color_eyre::install()?;

	println!("Testing ask_llm with simple prompt...");
	let response = ask_llm::oneshot("Say hello in one word").await?;
	println!("Success! Response: {}", response.text);

	Ok(())
}
