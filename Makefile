HYPERVISOR ?= cloud_hypervisor
GUESTOS_IMAGE ?= centos
WASM_RUNTIME ?= wasmedge
KERNEL_VERSION ?= 6.2
ARCH ?= x86_64
# DEST_DIR is used when built with RPM format
DEST_DIR ?= /
INSTALL_DIR := /var/lib/kuasar
BIN_DIR := /usr/local/bin
SYSTEMD_SERVICE_DIR := /usr/lib/systemd/system
SYSTEMD_CONF_DIR := /etc/sysconfig

.PHONY: vmm wasm quark clean all install-vmm install-wasm install-quark install

all: vmm quark wasm

bin/vmm-sandboxer:
	@cd vmm/sandbox && cargo build --release --bin ${HYPERVISOR}
	@mkdir -p bin && cp vmm/sandbox/target/release/${HYPERVISOR} bin/vmm-sandboxer

bin/vmm-task:
	@cd vmm/task && cargo build --release --target=${ARCH}-unknown-linux-musl
	@mkdir -p bin && cp vmm/task/target/${ARCH}-unknown-linux-musl/release/vmm-task bin/vmm-task

bin/vmlinux.bin:
	@bash vmm/scripts/kernel/${HYPERVISOR}/build.sh ${KERNEL_VERSION}
	@mkdir -p bin && cp vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin bin/vmlinux.bin && rm vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin

bin/kuasar.img:
	@bash vmm/scripts/image/${GUESTOS_IMAGE}/build.sh image
	@mkdir -p bin && cp /tmp/kuasar.img bin/kuasar.img && rm /tmp/kuasar.img

bin/kuasar.initrd:
	@bash vmm/scripts/image/${GUESTOS_IMAGE}/build.sh initrd
	@mkdir -p bin && cp /tmp/kuasar.initrd bin/kuasar.initrd && rm /tmp/kuasar.initrd

bin/wasm-sandboxer:
	@cd wasm && cargo build --release --features=${WASM_RUNTIME}
	@mkdir -p bin && cp wasm/target/release/wasm-sandboxer bin/wasm-sandboxer

bin/quark-sandboxer:
	@cd quark && cargo build --release
	@mkdir -p bin && cp quark/target/release/quark-sandboxer bin/quark-sandboxer

bin/runc-sandboxer:
	@cd runc && cargo build --release
	@mkdir -p bin && cp runc/target/release/runc-sandboxer bin/runc-sandboxer

wasm: bin/wasm-sandboxer
quark: bin/quark-sandboxer
runc: bin/runc-sandboxer

ifeq ($(HYPERVISOR), stratovirt)
vmm: bin/vmm-sandboxer bin/kuasar.initrd bin/vmlinux.bin
else
vmm: bin/vmm-sandboxer bin/kuasar.img bin/vmlinux.bin
endif

clean:
	@rm -rf bin
	@cd vmm/sandbox && cargo clean
	@cd vmm/task && cargo clean
	@cd wasm && cargo clean
	@cd quark && cargo clean
	@cd runc && cargo clean

install-vmm:
	@install -d -m 750 ${DEST_DIR}${BIN_DIR}
	@install -p -m 550 bin/vmm-sandboxer ${DEST_DIR}${BIN_DIR}/vmm-sandboxer
	@install -d -m 750 ${DEST_DIR}${INSTALL_DIR}
	@install -p -m 640 bin/vmlinux.bin ${DEST_DIR}${INSTALL_DIR}/vmlinux.bin
	@install -d -m 750 ${DEST_DIR}${SYSTEMD_SERVICE_DIR}
	@install -p -m 640 vmm/service/kuasar-vmm.service ${DEST_DIR}${SYSTEMD_SERVICE_DIR}/kuasar-vmm.service
	@install -d -m 750 ${DEST_DIR}${SYSTEMD_CONF_DIR}
	@install -p -m 640 vmm/service/kuasar-vmm ${DEST_DIR}${SYSTEMD_CONF_DIR}/kuasar-vmm

ifeq ($(HYPERVISOR), stratovirt)
	@install -p -m 640 bin/kuasar.initrd ${DEST_DIR}${INSTALL_DIR}/kuasar.initrd
	@install -p -m 640 vmm/sandbox/config_stratovirt_${ARCH}.toml ${DEST_DIR}${INSTALL_DIR}/config_stratovirt.toml
else
	@install -p -m 640 bin/kuasar.img ${DEST_DIR}${INSTALL_DIR}/kuasar.img
	@install -p -m 640 vmm/sandbox/config_clh.toml ${DEST_DIR}${INSTALL_DIR}/config_clh.toml
endif

install-wasm:
	@install -p -m 550 bin/wasm-sandboxer ${DEST_DIR}${BIN_DIR}/wasm-sandboxer
	@install -d -m 750 ${DEST_DIR}${SYSTEMD_SERVICE_DIR}
	@install -p -m 640 wasm/service/kuasar-wasm.service ${DEST_DIR}${SYSTEMD_SERVICE_DIR}/kuasar-wasm.service

install-quark:
	@install -p -m 550 bin/quark-sandboxer ${DEST_DIR}${BIN_DIR}/quark-sandboxer
	@install -d -m 750 ${DEST_DIR}${SYSTEMD_SERVICE_DIR}
	@install -p -m 640 quark/service/kuasar-quark.service ${DEST_DIR}${SYSTEMD_SERVICE_DIR}/kuasar-quark.service

install-runc:
	@install -p -m 550 bin/runc-sandboxer ${DEST_DIR}${BIN_DIR}/runc-sandboxer

install: all install-vmm install-wasm install-quark install-runc
