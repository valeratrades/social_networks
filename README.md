# social_networks
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/social_networks.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/social_networks)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/social_networks)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/social_networks-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/errors.yml?branch=master&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/warnings.yml?branch=master&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->

scripts to automate scraping from or certain parts of interactions with social networks.

has aggregators of sentiment polls from Twitter and Telegram, interpretation of Hamaha's video titles, discord /ping notifier, etc
<!-- markdownlint-disable -->
<details>
  <summary>
    <h3>Installation</h3>
  </summary>
<div class="markdown-content">```sh
cargo install --git https://github.com/valeratrades/social_networks --branch master # semantically `release` is preferrable, but I forget to push there sometimes
```

### Email Setup (Remote Server)
For the email service on a remote server, you need to authenticate locally first, then copy the token file:

1. Run `social_networks email` on your **local machine** (where you can open a browser)
2. Complete the OAuth authentication flow
3. Copy the token file to the remote server:
   ```sh
   scp ~/.local/state/social_networks/gmail_tokens.json <remote-server>:~/.local/state/social_networks/
   ```</div>
</details>
<!-- markdownlint-restore -->

## Usage
on the server, fill in the [~/.config/social_networks.toml], following, [../examples/config.toml]
Then startup needed services:
```sh
social_networks discord
social_networks telegram
social_networks twitter
social_networks youtube
```



<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting).
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

