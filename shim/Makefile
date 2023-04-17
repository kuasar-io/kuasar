all: bin/containerd-shim-kuasar-vmm-v2 bin/containerd-shim-kuasar-wasm-v2

bin/containerd-shim-kuasar-vmm-v2:
	@cargo build --release --bin containerd-shim-kuasar-vmm-v2
	@mkdir -p bin && cp target/release/containerd-shim-kuasar-vmm-v2 bin/containerd-shim-kuasar-vmm-v2

bin/containerd-shim-kuasar-wasm-v2:
	@cargo build --release --bin containerd-shim-kuasar-wasm-v2
	@mkdir -p bin && cp target/release/containerd-shim-kuasar-wasm-v2 bin/containerd-shim-kuasar-wasm-v2

clean:
	@rm -rf bin/
	@cargo clean

install: all
	@install -p -m 755 bin/containerd-shim-kuasar-vmm-v2 /usr/local/bin/containerd-shim-kuasar-vmm-v2
	@install -p -m 755 bin/containerd-shim-kuasar-wasm-v2 /usr/local/bin/containerd-shim-kuasar-wasm-v2

.PHONY: all clean install

