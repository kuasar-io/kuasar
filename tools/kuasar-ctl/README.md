# kuasar-ctl

`kuasar-ctl` is a diagnostic tool for Kuasar sandboxes.

The current version provides the `exec` subcommand, which executes commands inside a guest VM via the debug console and propagates the guest process's exit code back to the host `kuasar-ctl` process.

## Constraints and Limitations

Currently, only the `hvsock` debug console for Cloud Hypervisor is supported.

The following scenarios are not yet supported:

- QEMU `vsock://<cid>:<port>` debug channel
- StratoVirt `vsock://<cid>:<port>` debug channel
- Other backends that have not integrated `task.vsock`

The `exec` command will fail if the specified sandbox is not a Cloud Hypervisor instance or if `task.vsock` is missing.

For sandbox backends that are not yet supported by `kuasar-ctl`, continue to
use the existing manual debug-console workflow with `nc` or `ncat` over
`vsock`.

## Prerequisites

Before use, ensure that:

1. The target is a Cloud Hypervisor sandbox.
2. The guest has the debug console enabled.
3. A POSIX-compliant shell (e.g., `sh` or `bash`) is available in the guest image.
4. `task.vsock` exists in the sandbox directory.

If the debug console is not enabled, refer to the `task.debug` section in [docs/vmm/README.md](../../docs/vmm/README.md).

## Usage

Command format:

```bash
kuasar-ctl exec <sandbox-id-or-prefix> [--vport <port>] [--timeout <seconds>] -- <command> [args...]
```

Arguments:

- `<sandbox-id-or-prefix>`: Cloud Hypervisor sandbox ID or a unique prefix.
- `--vport`, `-p`: Debug console port (default is `1025`).
- `--timeout`, `-t`: Optional timeout in seconds.
- `<command> [args...]`: Command and arguments to execute in the guest.

## Examples

Execute a command using a full sandbox ID:

```bash
kuasar-ctl exec 5cbcf744949d8500e7159d6bd1e3894211f475549c0be15d9c60d3c502c7ede3 -- uname -a
```

Execute a command using a unique prefix:

```bash
kuasar-ctl exec 5cbcf744 -- sh -c 'echo hello from guest'
```

Execute with a timeout:

```bash
kuasar-ctl exec 5cbcf744 --timeout 5 -- sleep 10
```

Specify a debug console port:

```bash
kuasar-ctl exec 5cbcf744 --vport 1025 -- id
```

## Exit Codes

`kuasar-ctl exec` preserves the exit code semantics of the guest command wherever possible:

- When the guest command completes normally, the exit code of the guest command is returned.
- If interrupted by `Ctrl-C`, it returns `130`.
- If the command times out, it returns `124`.
- For handshake failures, missing sandboxes, missing `task.vsock`, or failures to parse the exit code, a non-zero error code is returned.

## Notes

1. `exec` relies on the debug console and is intended for diagnostic purposes. It is not suitable as a stable production execution interface.
2. The implementation automatically handles argument escaping and strips internal exit code tokens based on a dynamic token mechanism (without affecting standard output). However, it is still recommended to use well-defined and predictable commands.
3. The sandbox ID prefix must be unique. If multiple sandboxes match the prefix, the command will exit with an error.
4. Absolute paths provided as the sandbox parameter should point to a Cloud Hypervisor sandbox directory.
5. If the debug console is disabled in the guest, `kuasar-ctl exec` will be unavailable even if the sandbox is running.

## Future Extensibility

The current implementation utilizes the `SandboxTarget` structure. Support for other backends such as QEMU or StratoVirt (via `vsock`) can be added in the future without refactoring the command dispatch logic.
