# gcode-sender

A G-code sender for GRBL-family CNC controllers, driven entirely over an
HTTP API. It is built for the Sienci LongMill MK2 first and takes
[gSender](https://github.com/Sienci-Labs/gsender) as its reference for how
to talk to the machine.

There is no user interface. Clients upload a program, start it, and pause,
resume, or stop it with HTTP calls. The server owns the USB serial port.

## Supported controllers

The LongMill MK2 has shipped with two controller boards, and both work over
USB serial at 115200 baud:

| Board | Firmware | RX buffer |
| --- | --- | --- |
| LongBoard (Arduino based) | Grbl 1.1 | 128 bytes |
| SuperLongBoard (SLB) | grblHAL | 1024 bytes |

The server identifies the firmware with `$I` when it connects and reads the
RX buffer size from the `[OPT:...]` line, so no configuration is needed for
either board.

## Running

```sh
cargo run --release -p gcode-sender -- --port /dev/ttyACM0 --data-dir ./data
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--port` | (required) | Serial port of the controller |
| `--baud` | `115200` | Baud rate |
| `--listen` | `127.0.0.1:8080` | HTTP address; port `0` picks a free one |
| `--data-dir` | `./data` | Where jobs are stored |
| `--rx-reserve` | `8` | Bytes of the RX buffer left unused as a margin |
| `--poll-ms` | `200` | Status report interval |
| `--reconnect-ms` | `1000` | Delay between attempts to open the port |

Each option can also be set with an environment variable such as
`GCODE_SENDER_PORT`. Logging is controlled with `RUST_LOG`.

## HTTP API

All responses are JSON. Errors look like `{"error": "..."}` and use 404 for
an unknown job, 409 when the request does not fit the current state, 422
for an invalid program, and 503 when the machine is not connected or not
identified yet.

| Method and path | What it does |
| --- | --- |
| `GET /health` | Liveness check |
| `GET /machine` | Connection, firmware, live status, alarm, and the active job |
| `POST /jobs?name=<name>` | Upload a program; the body is the G-code text. Returns `201` |
| `GET /jobs` | List jobs, oldest first |
| `GET /jobs/{id}` | One job, with live progress while it runs |
| `DELETE /jobs/{id}` | Delete a job that is not active |
| `POST /jobs/{id}/start` | Start streaming. Also re-runs a finished job from the top |
| `POST /jobs/{id}/pause` | Feed hold, and stop sending lines |
| `POST /jobs/{id}/resume` | Cycle start once the hold has settled, and keep sending |
| `POST /jobs/{id}/stop` | Feed hold, then soft reset once the machine is at rest |

Only one job is active at a time. A job's `state` is one of `loaded`,
`running`, `paused`, `stopping`, `completed`, `stopped`, `failed`, or
`interrupted`. Records also carry `total_lines`, `acked_lines`,
`last_acked_line` (a line number in the uploaded file), `line_errors`, and
a `reason` for pauses and failures.

```sh
id=$(curl -s --data-binary @part.nc 'localhost:8080/jobs?name=part.nc' | jq -r .id)
curl -s -X POST localhost:8080/jobs/$id/start
curl -s localhost:8080/jobs/$id | jq '{state, acked_lines, total_lines}'
```

## How it works

- **Streaming.** Comments, blank lines, and `%` are stripped, then lines
  are sent with GRBL character counting: a line goes out only when it fits
  in what is left of the controller's RX buffer.
- **Completion.** A job completes when every line is acknowledged and the
  machine reports `Idle`, not when the last `ok` arrives.
- **Errors and alarms.** A line answered with `error:N` holds the machine
  and pauses the job, recording the file line number, so an operator can
  decide whether to resume or stop. An alarm fails the job.
- **Stopping.** The server sends a feed hold, waits for `Hold:0` (or
  `Idle`), and only then sends a soft reset, so the machine decelerates
  and keeps its position instead of raising `ALARM:3`.
- **State on disk.** Jobs live under `--data-dir` as the uploaded program
  plus a JSON record written with an atomic rename. Progress is saved
  every 250 ms while running. If the server stops while a job is active,
  the next server marks that job `interrupted` with its saved progress.

## Testing

The test suite is acceptance tests only. Each test starts a simulated
controller (`crates/grbl-sim`) on a pseudo-terminal and a real
`gcode-sender` process pointed at it, then drives the server over HTTP and
checks what the controller received. The simulator models the RX buffer,
planner queue, realtime commands, feed hold deceleration, soft reset,
alarms, and injected `error:N` replies, and flags any RX buffer overflow.

```sh
cargo test
GCODE_SENDER_TEST_LOG=1 cargo test -- --nocapture   # with server logs
```

The tests need Linux or macOS for pseudo-terminals.

## Not done yet

- Resuming an interrupted job from where it stopped, after a dropped
  connection or a server restart. This needs modal state (units, distance
  mode, work offset, spindle) rebuilt before the resume line.
- Optional X/Y/Z boundaries checked before lines are sent.
- Unlocking (`$X`) and homing (`$H`) endpoints, and jogging.
