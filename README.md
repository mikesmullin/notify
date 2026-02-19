# notify

Dispatches D-Bus desktop notifications.

## Motivation

- safer than `notify-send`, because text can be passed untrusted via stdin/file
- all params can be passed via YAML for convenience
- optionally await on user acknowledgement/choices
- custom notification card support (via structured JSON body) (ie. for [xnotid](https://github.com/mikesmullin/xnotid))

## Usage

```bash
notify [options] [summary] [body...]
```

- `summary` is optional notification title.
- `body...` captures all remaining words (quotes optional).
- Use `body` as `-` to read body text from stdin.
- Use `--file <path>` (or `--file -`) to read a YAML payload.

## Build

```bash
cargo build --release
```

## YAML input model

When stdin is piped (and `--file` is not provided), input is parsed as YAML.

```yaml
summary: Deploy status
body: |
  Build completed.
  Waiting for approval.
urgency: critical # low|normal|critical
icon: dialog-warning
app_name: notify
category: system
hints:
  transient: true
  desktop-entry: my-app
actions:
  - approve:Approve
  - deny:Deny
progress: 90
timeout: 0
id: 0
print_id: true
await: true
```

## Examples

Send from file:

```bash
notify --file payload.yaml
```

Send from stdin YAML:

```bash
printf 'summary: Test\nbody: Hello\n' | notify
```

Send with positional summary/body (unquoted body words are supported):

```bash
notify test whats up
```

Send body from stdin text using positional `-`:

```bash
echo "multi-line body" | notify "from stdin" -
```

Send interactive question and wait for user response:

```bash
notify --file question.yaml --timeout=0 \
  --action=approve:Approve --action=deny:Deny --await
```

Bound await time using `-t/--timeout`:

```bash
notify --file question.yaml --await --timeout=10000
```

`--await` prints JSON to stdout:

- action selected: `{"event":"action","id":123,"action":"approve"}`
- notification closed: `{"event":"closed","id":123,"reason":2}`
- await timeout: `{"event":"await-timeout","id":123,"timeout_ms":10000}`

When `--await` is set and `-t/--timeout` is provided, `notify` also applies a client-side wait cap of `timeout + 1000ms`.

If that await cap is reached, `notify` exits with code `124`.

## Notes

- CLI options override YAML fields.
- Inputs are treated as untrusted data.
- NUL bytes are stripped from text fields.
