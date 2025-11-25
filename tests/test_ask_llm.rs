use color_eyre::eyre::Result;

#[tokio::test]
async fn test_ask_llm_isolated() -> Result<()> {
	color_eyre::install()?;

	println!("Testing ask_llm with simple prompt...");
	let response = ask_llm::oneshot("Say hello in one word", ask_llm::Model::Fast).await?;
	println!("Success! Response: {}", response.text);

	Ok(())
}
