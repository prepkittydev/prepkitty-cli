# prepkitty CLI

Start, test and submit [PrepKitty](https://prepkitty.dev) exercises and projects on your own machine.

This repo is a read-only mirror of the CLI from the main PrepKitty repo. It also hosts the releases.

## Install

macOS / Linux:

```sh
curl -fsSL https://prepkitty.dev/install.sh | sh
```

Windows (PowerShell or Command Prompt):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://prepkitty.dev/install.ps1 | iex"
```

Open a new terminal afterwards so `prepkitty` is on your PATH. You'll also need Python 3.10+ (or a JDK 17+ for Java tracks).

## Use

```sh
prepkitty login                  # approve the CLI in your browser
prepkitty next                   # what to work on next
prepkitty start py-ex-greeting   # starter files in a local git repo
prepkitty test                   # run the tests as often as you like
git commit -am "solve" && prepkitty submit
```

`prepkitty status` shows your kitty and streak, and `prepkitty open` opens the current item in your browser.

## Build from source

```sh
cargo build --release
```

Set `PREPKITTY_API` to point the CLI at another server (for example `http://localhost:3000` when developing PrepKitty).

## License

MIT, see [LICENSE](LICENSE).
