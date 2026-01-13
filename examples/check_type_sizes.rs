use std::mem::size_of;

fn main() {
	println!("Type sizes in bytes:");
	println!("  grammers_client::Update: {}", size_of::<grammers_client::Update>());
	println!("  grammers_tl_types::enums::Update: {}", size_of::<grammers_tl_types::enums::Update>());
	println!("  grammers_tl_types::enums::Message: {}", size_of::<grammers_tl_types::enums::Message>());
	println!("  grammers_tl_types::types::Message: {}", size_of::<grammers_tl_types::types::Message>());
	println!("  grammers_tl_types::enums::MessageMedia: {}", size_of::<grammers_tl_types::enums::MessageMedia>());
	println!("  grammers_tl_types::enums::Chat: {}", size_of::<grammers_tl_types::enums::Chat>());
	println!("  grammers_tl_types::enums::User: {}", size_of::<grammers_tl_types::enums::User>());
	println!("  grammers_tl_types::types::UpdateNewMessage: {}", size_of::<grammers_tl_types::types::UpdateNewMessage>());
	println!("  grammers_tl_types::enums::MessageReplyHeader: {}", size_of::<grammers_tl_types::enums::MessageReplyHeader>());
	println!("  grammers_tl_types::enums::ReplyMarkup: {}", size_of::<grammers_tl_types::enums::ReplyMarkup>());

	println!("\nLarge difference-related types:");
	println!("  updates::Difference: {}", size_of::<grammers_tl_types::enums::updates::Difference>());
	println!("  updates::ChannelDifference: {}", size_of::<grammers_tl_types::enums::updates::ChannelDifference>());

	println!("\nDeserialize-related types (likely on stack during deserialization):");
	println!("  tl::enums::Updates: {}", size_of::<grammers_tl_types::enums::Updates>());
	println!("  tl::types::Updates: {}", size_of::<grammers_tl_types::types::Updates>());
	println!("  tl::types::UpdatesCombined: {}", size_of::<grammers_tl_types::types::UpdatesCombined>());

	println!("\nStack frame reference:");
	println!("  Default tokio stack: 2 MB = {} bytes", 2 * 1024 * 1024);
	println!("  Current configured:  8 MB = {} bytes", 8 * 1024 * 1024);

	let update_size = size_of::<grammers_tl_types::enums::Update>();
	println!("\nStack frame calculations:");
	println!("  Updates that fit in 2MB: {}", 2 * 1024 * 1024 / update_size);
	println!("  Updates that fit in 8MB: {}", 8 * 1024 * 1024 / update_size);
}
