// The conversation as `send` sees it. `shown`: how often the message's letters and digits run in the
// conversation, which is how it is recognised whatever the bubble renders emoji and line breaks as;
// `null` without a `main`. `front`: the topmost dialog, or without a composer a notice in `main`, with
// the first button of `PASS` it offers; `pin` when it asks for the end-to-end-encryption PIN.
([composer, message]) => {
	// most specific first: on "Continue without restoring?", Close and Cancel lead back to the PIN
	const PASS = ["Don't restore messages", 'Skip', 'Not now', 'Continue', 'OK', 'Got it', 'Dismiss', 'Decline optional cookies', 'Close'];
	const norm = s => (s ?? '').replaceAll('’', "'").replace(/\s+/g, ' ').trim();
	const letters = s => s.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');
	const visible = e => e.getClientRects().length > 0;
	const named = root => [...root.querySelectorAll('[role="button"], button')].filter(visible).map(b => norm(b.getAttribute('aria-label')) || norm(b.innerText));
	const offered = (root, text) => {
		const names = named(root);
		return { text: norm(text).slice(0, 500), buttons: names, pass: PASS.find(p => names.includes(p)) ?? null, pin: !!root.querySelector('input[aria-label="PIN"]') };
	};
	const boxes = document.querySelectorAll(composer);
	const main = document.querySelector('[role="main"]');
	const dialogs = [...document.querySelectorAll('[role="dialog"], [role="alertdialog"]')].filter(visible);
	const top = dialogs.at(-1);
	let front = null;
	if (top) front = { kind: 'dialog', ...offered(top, top.innerText) };
	else if (!boxes.length && main) {
		const notice = offered(main, main.innerText);
		if (notice.pass) front = { kind: 'notice', ...notice };
	}
	return {
		composers: boxes.length,
		draft: boxes[0]?.textContent ?? '',
		text: document.body.innerText.toLowerCase().replaceAll('’', "'"),
		shown: main ? letters(main.innerText).split(letters(message)).length - 1 : null,
		front,
	};
}
