## Luminous TTV
A [Rust][rust] server to retrieve and relay a playlist for Twitch livestreams/VODs.

By running this server, and using [a browser extension][ext] to relay certain requests to it, Twitch will not
display any ads.

[rust]: https://www.rust-lang.org

### How it works

The server makes use of the [Hola](https://en.wikipedia.org/wiki/Hola_(VPN))
proxy network to connect to Twitch via Russia. Twitch does not currently
serve any livestream ads to users in Russia.

* This server doesn't use your actual Twitch ID, it generates its own.
* You will not be acting as a peer of the Hola network.

### Building

As an alternative to building, you can [download pre-built releases][release].
Just unzip it anywhere and run `luminous-ttv`

You'll also need to add [the browser extension][ext] to your browser so that
requests get routed.

---

1. [Install Rust](https://rustup.rs/).
2. Run `cargo install --locked --git https://github.com/AlyoshaVasilieva/luminous-ttv.git`
3. Run `luminous-ttv`

[ext]: https://github.com/AlyoshaVasilieva/luminous-ttv-ext
[release]: https://github.com/AlyoshaVasilieva/luminous-ttv/releases/latest

### Issues

* Loading streams takes longer, up to around 10 seconds. (This doesn't affect
  the latency.)
* In Firefox, and browsers built using its code, the extension's error fallback code 
  can't be used. If this program isn't running, you won't be able to view any streams
  on Twitch. (In Chrome-like browsers, the extension will fall back to the
  ad-filled stream when anything goes wrong.)

### Possible issues

1. Hola might ban you, which will make this stop working unless you have
   your own Russian proxy to use.
2. Hola might stop running servers in Russia.
3. Twitch might start serving ads to users in Russia.
4. This will cause you to load the video from Europe (Sweden for me) which may
   cause buffering issues if your internet isn't that good and that's far away.
   It doesn't cause an issue for me beyond maybe 1 second of additional latency
   due to repeatedly crossing an ocean.

### License

GNU GPLv3 as a whole. The file `hello.rs` is available under the MIT license, as it
is a partial port of an [MIT-licensed project](https://github.com/Snawoot/hola-proxy).
