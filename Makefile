HYPERVISOR ?= cloud_hypervisor
GUESTOS_IMAGE ?= centos
WASM_RUNTIME ?= wasmedge
KERNEL_VERSION ?= 6.2

.PHONY: vmm wasm quark clean all install-vmm install-wasm install-quark install 

all: vmm quark wasm

bin/vmm-sandboxer:
	@cd vmm && cargo build --release --features=${HYPERVISOR}
	@mkdir -p bin && cp vmm/target/release/vmm-sandboxer bin/vmm-sandboxer 

bin/vmm-task:
	@cd vmm/task && cargo build --release --target=x86_64-unknown-linux-musl
	@mkdir -p bin && cp vmm/target/x86_64-unknown-linux-musl/release/vmm-task bin/vmm-task

bin/vmlinux.bin:
	@bash -x vmm/scripts/kernel/${HYPERVISOR}/build.sh ${KERNEL_VERSION}
	@mkdir -p bin && cp vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin bin/vmlinux.bin && rm vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin

bin/kuasar.img:
	@bash -x vmm/scripts/image/${GUESTOS_IMAGE}/build.sh image
	@mkdir -p bin && cp /tmp/kuasar.img bin/kuasar.img && rm /tmp/kuasar.img

bin/kuasar.initrd:
	@bash -x vmm/scripts/image/${GUESTOS_IMAGE}/build.sh initrd
	@mkdir -p bin && cp /tmp/kuasar.initrd bin/kuasar.initrd && rm /tmp/kuasar.initrd

bin/wasm-sandboxer:
	@cd wasm && cargo build --release --features=${WASM_RUNTIME}
	@mkdir -p bin && cp wasm/target/release/wasm-sandboxer bin/wasm-sandboxer

bin/quark-sandboxer:
	@cd quark && cargo build --release
	@mkdir -p bin && cp quark/target/release/quark-sandboxer bin/quark-sandboxer

wasm: bin/wasm-sandboxer
quark: bin/quark-sandboxer

ifeq ($(HYPERVISOR), stratovirt)
vmm: bin/vmm-sandboxer bin/kuasar.initrd bin/vmlinux.bin
else
vmm: bin/vmm-sandboxer bin/kuasar.img bin/vmlinux.bin
endif

clean:
	@rm -rf bin
	@cd vmm && cargo clean
	@cd wasm && cargo clean
	@cd quark && cargo clean

ifeq ($(HYPERVISOR), stratovirt)
install-vmm:
	@install -p -m 755 bin/vmm-sandboxer /usr/local/bin/vmm-sandboxer
	@install -d /var/lib/kuasar
	@install -p -m 644 bin/vmlinux.bin /var/lib/kuasar/vmlinux.bin
	@install -p -m 644 bin/kuasar.initrd /var/lib/kuasar/kuasar.initrd
	@install -p -m 644 vmm/sandbox/config_stratovirt.toml /var/lib/kuasar/config_stratovirt.toml
else
install-vmm:
	@install -p -m 755 bin/vmm-sandboxer /usr/local/bin/vmm-sandboxer
	@install -d /var/lib/kuasar
	@install -p -m 644 bin/vmlinux.bin /var/lib/kuasar/vmlinux.bin
	@install -p -m 644 bin/kuasar.img /var/lib/kuasar/kuasar.img
	@install -p -m 644 vmm/sandbox/config_clh.toml /var/lib/kuasar/config_clh.toml
endif

install-wasm:
	@install -p -m 755 bin/wasm-sandboxer /usr/local/bin/wasm-sandboxer

install-quark:
	@install -p -m 755 bin/quark-sandboxer /usr/local/bin/quark-sandboxer

install: all install-vmm install-wasm install-quark