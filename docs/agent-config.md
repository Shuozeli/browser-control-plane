# Agent Profile Config

`bcp-agent` discovers local browser profiles from a TOML config file.

Discovery order:

1. `--config-path`
2. `BCP_AGENT_CONFIG`
3. `.bcp/agent.toml`, when it exists
4. env fallback: `BCP_REAL_PROFILES` or `BCP_E2E_*`
5. SQLite fallback: `BCP_AGENT_DB`

Example:

```toml
machine_id = "mac-mini-1"
gateway = "recording" # recording or real-pwright

[labels]
site = "home"
owner = "media-team"

[[profiles]]
profile_id = "yt-main"
account_id = "yt-main"
platform = "youtube"
profile_path = "/Users/me/ChromeProfiles/yt-main"
display_name = "YouTube Main"
cdp_url = "http://127.0.0.1:9222"
initial_url = "https://studio.youtube.com"
capabilities = ["snapshot", "click", "eval"]

[profiles.labels]
tier = "prod"

[profiles.lifecycle]
launch_command = [
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "--remote-debugging-port=9222",
  "--user-data-dir=/Users/me/ChromeProfiles/yt-main"
]
working_dir = "/Users/me"

[profiles.lifecycle.env]
CHROME_LOG_FILE = "/tmp/yt-main-chrome.log"
```

Notes:

- `machine_id` is the machine identity reported to the global controller.
- `gateway = "recording"` is for fake/local tests.
- `gateway = "real-pwright"` requires `bcp-agent --features real-pwright`.
- `profile_path` is durable local identity for the Chrome profile.
- `cdp_url` is the local CDP endpoint that the pwright layer uses.
- `initial_url` is used by real pwright when it opens a tab.
- `lifecycle` is the local browser process declaration. When present, the
  agent starts `launch_command` before `check_browser` or `ensure_browser`,
  restarts it if the child exits, and kills it during `stop_browser`. The
  profile is also marked with `bcp.lifecycle = managed`.

Multiple accounts can be attached to one profile:

```toml
[[profiles]]
profile_id = "shared-main"
profile_path = "/Users/me/ChromeProfiles/shared-main"
cdp_url = "http://127.0.0.1:9223"

[[profiles.accounts]]
account_id = "yt-shared"
platform = "youtube"
handle = "@yt-shared"
health = "logged_in"
capabilities = ["snapshot", "click"]

[[profiles.accounts]]
account_id = "x-shared"
platform = "x"
handle = "@x-shared"
health = "logged_in"
capabilities = ["snapshot", "click"]
```
