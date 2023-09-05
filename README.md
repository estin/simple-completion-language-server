<div align="center">
  <p><h1>simple-completion-language-server</h1> </p>
  <p><strong>Allow to use common word completion and snippets for <a href="https://helix-editor.com/">Helix editor</a></strong></p>
  <p></p>
</div>


### Install (from source only)

```bash
$ git clone https://github.com/estin/simple-completion-language-server.git
$ cd simple-completion-language-server
$ cargo install --path .
```


### Configure

For Helix on ~/.config/helix/languages.toml

```toml
# introudce new language server
# - set max completion results len to 20
# - write logs to /tmp/completion.log
[language-server]
scls = { command = "simple-completion-language-server", config = { "max_completion_items" = 20 }, environment = { "RUST_LOG" = "debug,simple-completion-langauge-server=debug",  "LOG_FILE" = "/tmp/completion.log" } }

# introduce new language to enable completion
# :set-language stub
[[language]]
name = "stub"
scope = "text.stub"
file-types = []
shebangs = []
roots = []
auto-format = false
language-servers = [ "scls" ]

# append langage server to existed languages
[[language]]
name = "rust"
language-servers = [ "scls", "rust-analyzer" ]

[[language]]
name = "markdown"
language-servers = [ "scls", "marksman" ]

[[language]]
name = "html"
language-servers = [ "scls", "vscode-html-language-server" ]

[[language]]
name = "toml"
language-servers = [ "scls", "taplo" ]

[[language]]
name = "dockerfile"
language-servers = [ "scls", "docker-langserver" ]

[[language]]
name = "git-commit"
language-servers = [ "scls" ]

# etc..
```


### Snippets

Read snippets from dir `~/.config/helix/snippets` or specify snippets path via `SNIPPETS_PATH` env.

Currently supports our own `toml` format and vscode `json` (a basic effort)

Filename used as snippet scope (language), filename `snippets.(toml|json)` will not attach scope to snippets.

For example, snippets with filename `python.toml` or `python.json` would have a `python` scope.

Snippets format


```toml
[[snippets]]
prefix = "author"            # prefix to trigger snippet
body = "Evgeniy Tatarkin"    # snippet
description = "Author"       # (optional) description

[[snippets]]
prefix = "ld"
scope = [ "python" ]
body = 'log.debug("$1")'
```
