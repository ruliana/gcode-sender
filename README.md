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
| `--silence-timeout-ms` | `3000` | Treat the link as dead after this long without hearing from the controller |
| `--ack-timeout-ms` | `30000` | Treat an acknowledgement as lost after lines stay unacknowledged this long while the machine is idle. Keep it above your longest dwell (`G4`) |

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
| `POST /jobs/{id}/resume` | Cycle start once the hold has settled, and keep sending. Also continues an `interrupted`, `stopped` or `failed` job, with options; see below |
| `POST /jobs/{id}/stop` | Feed hold, then soft reset once the machine is at rest |

| `POST /machine/unlock` | `$X`: clear an alarm lock |
| `POST /machine/home` | `$H`: run the homing cycle |
| `POST /machine/reset` | Soft reset, for example to clear a hold left by an earlier session |
| `POST /machine/jog` | Move the tool by hand, see below |
| `POST /machine/jog/cancel` | Stop a jog in progress and drop the queued ones |
| `GET /machine/bounds` | The machine boundaries, see below |
| `PUT /machine/bounds` | Set the boundaries |
| `DELETE /machine/bounds` | Remove the boundaries |

The `/machine` actions are refused while a job is active; stop the job
first.

Only one job is active at a time. A job's `state` is one of `loaded`,
`running`, `paused`, `stopping`, `completed`, `stopped`, `failed`, or
`interrupted`. Records also carry `total_lines`, `acked_lines`,
`last_acked_line` (a line number in the uploaded file), `line_errors`, and
a `reason` for pauses and failures. A rejected line is recorded with its
error code and what the code means, and alarms are explained in `reason`
and in `GET /machine`.

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
  decide whether to resume or stop. An alarm fails the job; clear it with
  `POST /machine/unlock` or `POST /machine/home`.
- **Stopping.** The server sends a feed hold, waits for `Hold:0` (or
  `Idle`), and only then sends a soft reset, so the machine decelerates
  and keeps its position instead of raising `ALARM:3`.
- **State on disk.** Jobs live under `--data-dir` as the uploaded program
  plus a JSON record written with an atomic rename. Progress is saved
  every 250 ms while running.

### Interrupted jobs and resuming

A job becomes `interrupted` when the serial connection drops, the
controller stops answering, the controller resets, or the server stops
while the job is active. The server keeps reconnecting by itself.

`POST /jobs/{id}/resume` continues an `interrupted`, `stopped` or `failed`
job once the machine is connected, `Idle`, and not in alarm (clear one with
`POST /machine/unlock`). With no body it does three things:

1. **Backs up.** The saved progress can lag the controller, and a reset or
   stop throws away whatever was still in its planner, so the job restarts
   a little before the last acknowledged line: the controller's planner
   depth plus one line. The job record's `resumed_from_line` says where.
2. **Restores modal state.** Before the program continues, the server sends
   the units, distance mode, plane, work offset, feed mode, coolant,
   spindle, and feed rate the program had at that point.
3. **Moves straight to the next line.** There is no retract by default, so
   check where the tool is before you resume.

Two options, sent as a JSON body, make it safer:

```sh
# Lift, travel above the work, bring the spindle back, and plunge.
curl -X POST localhost:8080/jobs/$id/resume -H 'content-type: application/json' \
  -d '{"retract": {"z": -5, "dwell_s": 3}}'

# Start from a file line you choose, with no backing up.
curl -X POST localhost:8080/jobs/$id/resume -H 'content-type: application/json' \
  -d '{"from_line": 120}'
```

- `retract.z` is the safe height in machine coordinates, in millimetres.
  The server sends, in order: a lift to that Z, a rapid to the XY of the
  resume point, the coolant and spindle the program had, an optional
  `dwell_s` wait for the spindle (keep it below `--ack-timeout-ms`), a
  plunge back to the resume point's Z at `plunge_feed` (millimetres per
  minute; by default the program's own feed rate), and then the rest of the
  modal state. These moves are in machine coordinates and millimetres, so a
  program in inches or with another work offset cannot misplace them.
- A retract has to go up. It is refused with `422` if `z` is below the
  tool's current Z or below the resume point's Z, which catches a sign
  mistake, and if it is outside a Z boundary.
- `from_line` is a file line number. A comment or blank line means the next
  line of code, and `0` means the top of the program.
- Neither option applies to a paused job; plain `resume` continues those.

Repeating absolute moves is harmless; repeating incremental ones is not.
A program in `G91` at the resume point is refused with `422`, as is one
that uses `G92`, `G10`, `G28`, `G30`, or a work offset other than `G54`
before that point, because the server cannot rebuild that state. Those
jobs can still be started again from the top.

If the controller resets while moving (an Arduino based LongBoard does when
the USB port reopens), it raises `ALARM:3` and may have lost its position.
The server will not resume until you have dealt with the alarm.

### Watching the connection

- A status query goes out every `--poll-ms`. The controller answers even
  in the middle of a move, so total silence for `--silence-timeout-ms`
  means the link is dead. The server drops the connection and reconnects.
- If lines stay unacknowledged for `--ack-timeout-ms` while the machine is
  `Idle`, an acknowledgement was lost and nobody knows what the controller
  has received. The job fails and the controller is reset so nothing
  unknown is left in it.
- Line noise and unrecognised output are ignored. Acknowledgements left
  over from an earlier session are discarded while the controller is
  identified, so they cannot be mistaken for the new session's replies.

### Jogging

`POST /machine/jog` sends a Grbl `$J=` jog, which the controller runs
as a short move that can be cancelled at any moment. Distances and feed are
in millimetres and millimetres per minute.

```sh
# Move 10 mm along X and 2.5 mm back along Y from where the tool is.
curl -X POST localhost:8080/machine/jog -H 'content-type: application/json' \
  -d '{"x": 10, "y": -2.5, "feed": 1500}'

# Go to X0 Y0 in work coordinates, or add "space": "machine" for machine
# coordinates.
curl -X POST localhost:8080/machine/jog -H 'content-type: application/json' \
  -d '{"x": 0, "y": 0, "feed": 3000, "mode": "absolute"}'

curl -X POST localhost:8080/machine/jog/cancel
```

`mode` is `relative` (the default) or `absolute`; `space` (`work`, the
default, or `machine`) only applies to absolute jogs. The reply is `202`
once the controller has accepted the move, not when it finishes: watch
`GET /machine`, where the state is `Jog` while it runs. A jog the
controller refuses comes back with its error, for example `422` for
error:15 (the move leaves the machine's travel). Jogs sent while one is
running queue behind it, and cancelling drops them all. The cancel is a
realtime byte, so the machine does not reset and keeps its position.

Jogging is refused (`409`) during a job, an alarm, a hold or any other
state that is not idle, and (`503`) while disconnected. When boundaries are
set, the destination is checked first, counting the jogs still queued, and a
jog that leaves them is refused with `422`. As with jobs, this is on top of
the controller's soft limits, not instead of them.

### Boundaries

`PUT /machine/bounds` takes ranges in machine coordinates, in millimetres.
Leave an axis out to leave it unchecked.

```sh
curl -X PUT localhost:8080/machine/bounds -H 'content-type: application/json' \
  -d '{"x": [0, 800], "y": [0, 400], "z": [-60, 0]}'
```

When boundaries are set, a job is checked before any of it is sent, on
start and on resume. The whole program is followed from the machine's
current position, using the work offset the controller reports. Units,
absolute and incremental moves, `G53`, and arcs (including their bulge
between the end points, in the center and radius forms) are understood. A
move outside the boundaries is refused with `422` naming the line, the
axis and the value, and nothing reaches the controller. A program the
server cannot follow (`G92`, `G10`, `G28`, `G30`, other work offsets,
parameters, expressions) is refused too, because it could move anywhere.
These checks apply only while boundaries are set.

This complements the controller's own soft limits rather than replacing
them: it catches a bad program before it starts, but it knows nothing
about a tool length offset or a probe cycle that moves further than
planned.

## Testing

The test suite is acceptance tests only. Each test starts a simulated
controller (`crates/grbl-sim`) on a pseudo-terminal and a real
`gcode-sender` process pointed at it, then drives the server over HTTP and
checks what the controller received. The simulator models the RX buffer,
planner queue, realtime commands, feed hold deceleration, soft reset,
alarms, and the work offset. It can also unplug and replug the cable (with
or without the reset an Arduino board performs), go silent, lose
acknowledgements, inject line noise and `error:N` replies, and it flags any
RX buffer overflow.

```sh
cargo test
GCODE_SENDER_TEST_LOG=1 cargo test -- --nocapture   # with server logs
```

The tests need Linux or macOS for pseudo-terminals.

## Not done yet

- Probing and overrides.
- Reading and changing the controller's `$` settings.
- Resuming incremental programs.
- Testing against a real LongMill. The simulator follows the Grbl 1.1
  documentation and gSender's behaviour, but real hardware has quirks.
