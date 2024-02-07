# scheduledexec

REST service to execute command lines at appropriate moments using POST requests and monitor status of those commands.

## Features

* Executing command lines supplied by REST client at relative moments of time specified in the scenario
* Collecting and reporting outputs and timing of the commands
* Ability to prepend a program name to the command line array (to avoid giving access to the whole shell)
* Flexible server socket listening options (e.g. UNIX socket or systemd socket-initation)

## Limitations

* No support for non-UTF8 command line chunks or outputs
* Unsuccessful commands abort scenarios
* If command takes too long to execute and scheduledexec missed the next command, scenario is also aborted error

## Documentation

Here is a short list of endpoints:

```
/start - begin executing a scenario
/abort - abort currenty executed scenario
/status - query information
/monitor - subscribe to events     
/getscenario - get current or last scenario
/report - download final scenario report
```

You can use OpenAPI editor like [Swagger](https://editor.swagger.io/) with a [pre-built openapi.json](openapi.json) file to see the documentation.


## Example

```
$ cargo run  -- 127.0.0.1:1234
$ curl http://127.0.0.1:1234/status; echo
{"status":"Idle"}
$ curl -H 'Content-Type: application/json' -d @scenario_sample.json -s http://127.0.0.1:1234/start
$ curl http://127.0.0.1:1234/status; echo
{"status":"Completed"}
$ curl http://127.0.0.1:1234/report; echo
{"error":false,"aborted":false,"entries":[{"out":"Wed Feb  7 22:44:24 CET 2024\n","err":"","exitcode":0,"timespan_ms":3},...]}
```

## Installation

Download a pre-built executable from [Github releases](https://github.com/vi/scheduledexec/releases) or install from source code with `cargo install --path .`  or `cargo install scheduledexec`.

## CLI options

<details><summary> scheduledexec --help output</summary>

```
REST service to execute series of command lines at specific moments of time

Usage: 

Arguments:
  <LISTEN_ADDRESS>
          Socket address to listen for incoming connections.
          
          Various types of addresses are supported:
          
          * TCP socket address and port, like 127.0.0.1:8080 or [::]:80
          
          * UNIX socket path like /tmp/mysock or Linux abstract address like @abstract
          
          * Special keyword "inetd" for serving one connection from stdin/stdout
          
          * Special keyword "sd-listen" or "sd-listen-unix" to accept connections from file descriptor 3 (e.g. systemd socket activation)

  [PREFIX]
          Prepend this command to all executed chunks

Options:
      --unix-listen-unlink
          remove UNIX socket prior to binding to it

      --unix-listen-chmod <UNIX_LISTEN_CHMOD>
          change filesystem mode of the newly bound UNIX socket to `owner`, `group` or `everybody`

      --unix-listen-uid <UNIX_LISTEN_UID>
          change owner user of the newly bound UNIX socket to this numeric uid

      --unix-listen-gid <UNIX_LISTEN_GID>
          change owner group of the newly bound UNIX socket to this numeric uid

      --sd-accept-ignore-environment
          ignore environment variables like LISTEN_PID or LISTEN_FDS and unconditionally use file descritor `3` as a socket in sd-listen or sd-listen-unix modes

      --tcp-keepalive <TCP_KEEPALIVE>
          set SO_KEEPALIVE settings for each accepted TCP connection.
          
          Value is a colon-separated triplet of time_ms:count:interval_ms, each of which is optional.

      --tcp-reuse-port
          Try to set SO_REUSEPORT, so that multiple processes can accept connections from the same port in a round-robin fashion

      --recv-buffer-size <RECV_BUFFER_SIZE>
          Set socket's SO_RCVBUF value

      --send-buffer-size <SEND_BUFFER_SIZE>
          Set socket's SO_SNDBUF value

      --tcp-only-v6
          Set socket's IPV6_V6ONLY to true, to avoid receiving IPv4 connections on IPv6 socket

      --tcp-listen-backlog <TCP_LISTEN_BACKLOG>
          Maximum number of pending unaccepted connections

  -h, --help
          Print help (see a summary with '-h')
```
</details>
