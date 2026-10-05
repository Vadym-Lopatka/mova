# Security

## Run only on a trusted machine

The nREPL server runs any code a connected client sends. It runs with
the rights of the user who started it. It has no authentication.

- It binds to 127.0.0.1 by default (`-b` / `--bind` changes this).
- Do not bind it to a public address. Do not expose its port.
- If more than one user shares the machine, use a Unix socket
  (`-s` / `--socket`) or TLS (`--tls-keys-file`). TLS needs the `tls`
  cargo feature.

## Report a vulnerability

Use GitHub private vulnerability reporting on this repository.
Open the "Security" tab and choose "Report a vulnerability".
