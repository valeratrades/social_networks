/// Surface-level event yielded by a DM adapter (Telegram, Discord, ...).
///
/// Adapters parse platform-specific protocols (MTProto, Discord WS) into these neutral
/// records. They make no decisions about whether to notify — that is the consumer's job.
///
/// `mentions_me` and `is_reply_to_me` are platform-specific signals computed by the adapter
/// because the parsing is platform-specific (Discord mention substring scan, reply
/// reference traversal, etc.). For platforms where the concept does not apply (e.g. Telegram
/// DMs), the adapter sets both `false`.
#[derive(Clone, Debug)]
pub enum DmEvent {
	Message {
		platform: &'static str,
		sender: String,
		text: String,
		/// Stable key for throttling (chat/channel id as a string). Format is per-platform.
		chat_id: String,
		is_dm: bool,
		mentions_me: bool,
		is_reply_to_me: bool,
	},
	IncomingCall {
		platform: &'static str,
	},
}
