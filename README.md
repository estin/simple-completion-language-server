<div align="center">
  <p><h1>simple-completion-language-server</h1> </p>
  <p><strong>Allow to use common word completion for <a href="https://helix-editor.com/">Helix editor</a></strong></p>
  <p></p>
</div>


Install (from source only)
```bash
$ git clone https://github.com/estin/simple-completion-language-server.git
$ cd simple-completion-language-server
$ cargo install --path .
```


Configure Helix on ~/.config/helix/languages.toml
```toml
# introudce new language server
# - set max completion results len to 20
# - write logs to /tmp/completion.log
[language-server]
simple-completion-language-server = { command = "simple-completion-language-server", config = { "max_completion_items" = 20 }, environment = { "RUST_LOG" = "debug,simple-completion-langauge-server=debug",  "LOG_FILE" = "/tmp/completion.log" } }

# introduce new language to enable completion
# :set-language stub
[[language]]
name = "stub"
scope = "text.stub"
file-types = []
shebangs = []
roots = []
auto-format = false
language-servers = [ "simple-completion-language-server" ]

# append langage server to existed languages
[[language]]
name = "rust"
language-servers = [ "simple-completion-language-server", "rust-analyzer" ]

[[language]]
name = "markdown"
language-servers = [ "simple-completion-language-server", "marksman" ]

[[language]]
name = "html"
language-servers = [ "simple-completion-language-server", "vscode-html-language-server" ]

[[language]]
name = "toml"
language-servers = [ "simple-completion-language-server", "taplo" ]

[[language]]
name = "dockerfile"
language-servers = [ "simple-completion-language-server", "docker-langserver" ]

[[language]]
name = "git-commit"
language-servers = [ "simple-completion-language-server" ]

# etc..
```
