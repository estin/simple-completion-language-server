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

For Helix on `~/.config/helix/languages.toml`

```toml
# introudce new language server
# - set max completion results len to 20
# - completions will return before snippets by default
[language-server.scls]
command = "simple-completion-language-server"
config = { max_completion_items = 20, snippets_first = false }

# write logs to /tmp/completion.log
[language-server.scls.environment]
RUST_LOG = "debug,simple-completion-langauge-server=debug"
LOG_FILE = "/tmp/completion.log"

# append langage server to existed languages
[[language]]
name = "rust"
language-servers = [ "scls", "rust-analyzer" ]

[[language]]
name = "git-commit"
language-servers = [ "scls" ]

# etc..

# introduce a new language to enable completion on any doc by forcing language with :set-language stub
[[language]]
name = "stub"
scope = "text.stub"
file-types = []
shebangs = []
roots = []
auto-format = false
language-servers = [ "scls" ]
```

### Snippets

Read snippets from dir `~/.config/helix/snippets` or specify snippets path via `SNIPPETS_PATH` env.

Currently, it supports our own `toml` format and vscode `json` (a basic effort).

Filename used as snippet scope (language), filename `snippets.(toml|json)` will not attach scope to snippets.

For example, snippets with the filename `python.toml` or `python.json` would have a `python` scope.

Snippets format

```toml
[[snippets]]
prefix = "ld"
scope = [ "python" ]
body = 'log.debug("$1")'
```

### Use external snippets collections from git repos

Configure sources in `~/.config/helix/external-snippets.toml` (or via env `EXTERNAL_SNIPPETS_CONFIG`)

```toml
[[sources]] # list of sources to load
name = "friendly-snippets"  # optional name shown on snippet description
git = "https://github.com/rafamadriz/friendly-snippets.git" # git repo with snippets collections

[[sources.paths]] # list of paths to load on current source
scope = ["python"]  # optional scopes for current snippets
path = "snippets/python/python.json"  # where snippet file or dir located in repo
```


Clone or update snippets source repos to `~/.config/helix/external-snippets/<repo path>`

```bash
$ simple-completion-language-server fetch-external-snippets
```


Validate snippets

```bash
$ simple-completion-language-server validate-snippets
```


### Similar projects

- [metafates/buffer-language-server](https://github.com/metafates/buffer-language-server)
- [rajasegar/helix-snippets-ls](https://github.com/rajasegar/helix-snippets-ls)
- [quantonganh/snippets-ls](https://github.com/quantonganh/snippets-ls)
- [Stanislav-Lapata/snippets-ls](https://github.com/Stanislav-Lapata/snippets-ls)
- ...(please add another useful links here)

### Useful snippets collections

- [rafamadriz/friendly-snippets](https://github.com/rafamadriz/friendly-snippets)
- ...(please add another useful links here)
