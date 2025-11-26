```sh
cargo install --git https://github.com/valeratrades/social_networks --branch master # semantically `release` is preferrable, but I forget to push there sometimes
```

### Email Setup (Remote Server)
For the email service on a remote server, you need to authenticate locally first, then copy the token file:

1. Run `social_networks email` on your **local machine** (where you can open a browser)
2. Complete the OAuth authentication flow
3. Copy the token file to the remote server:
   ```sh
   scp ~/.local/state/social_networks/gmail_tokens.json <remote-server>:~/.local/state/social_networks/
   ```
