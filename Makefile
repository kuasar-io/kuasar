HYPERVISOR ?= cloud_hypervisor
GUESTOS_IMAGE ?= centos
WASM_RUNTIME ?= wasmedge
KERNEL_VERSION ?= 6.12.8
ARCH ?= x86_64
# DEST_DIR is used when built with RPM format
DEST_DIR ?= /
INSTALL_DIR := /var/lib/kuasar
BIN_DIR := /usr/local/bin
SYSTEMD_SERVICE_DIR := /usr/lib/systemd/system
SYSTEMD_CONF_DIR := /etc/sysconfig
ENABLE_YOUKI ?= false
RUNC_FEATURES =
VMM_TASK_FEATURES =
VMM_SANDBOX_FEATURES =

ifeq ($(ENABLE_YOUKI), true)
	RUNC_FEATURES = youki
	VMM_TASK_FEATURES = youki
endif

.PHONY: vmm wasm quark clean all install-vmm install-wasm install-quark install \
        bin/vmm-sandboxer bin/vmm-task bin/vmlinux.bin bin/kuasar.img bin/kuasar.initrd \
        bin/wasm-sandboxer bin/quark-sandboxer bin/runc-sandboxer \
        test-e2e test-e2e-framework verify-e2e local-up clean-e2e help

all: vmm quark wasm

bin/vmm-sandboxer:
	@cd vmm/sandbox && cargo build --release --bin ${HYPERVISOR} --features=${VMM_SANDBOX_FEATURES}
	@mkdir -p bin && cp target/release/${HYPERVISOR} bin/vmm-sandboxer

bin/vmm-task:
	@cd vmm/task && cargo build --release --target=${ARCH}-unknown-linux-musl --features=${VMM_TASK_FEATURES}
	@mkdir -p bin && cp target/${ARCH}-unknown-linux-musl/release/vmm-task bin/vmm-task

bin/vmlinux.bin:
	@bash vmm/scripts/kernel/${HYPERVISOR}/build.sh ${KERNEL_VERSION}
	@mkdir -p bin && cp vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin bin/vmlinux.bin && rm vmm/scripts/kernel/${HYPERVISOR}/vmlinux.bin

bin/kuasar.img:
	@bash vmm/scripts/image/build.sh image ${GUESTOS_IMAGE}
	@mkdir -p bin && cp /tmp/kuasar.img bin/kuasar.img && rm /tmp/kuasar.img

bin/kuasar.initrd:
	@bash vmm/scripts/image/build.sh initrd ${GUESTOS_IMAGE}
	@mkdir -p bin && cp /tmp/kuasar.initrd bin/kuasar.initrd && rm /tmp/kuasar.initrd

bin/wasm-sandboxer:
	@cd wasm && cargo build --release --features=${WASM_RUNTIME}
	@mkdir -p bin && cp target/release/wasm-sandboxer bin/wasm-sandboxer

bin/quark-sandboxer:
	@cd quark && cargo build --release
	@mkdir -p bin && cp target/release/quark-sandboxer bin/quark-sandboxer

bin/runc-sandboxer:
	@cd runc && cargo build --release --features=${RUNC_FEATURES}
	@mkdir -p bin && cp target/release/runc-sandboxer bin/runc-sandboxer

wasm: bin/wasm-sandboxer
quark: bin/quark-sandboxer
runc: bin/runc-sandboxer

ifeq ($(HYPERVISOR), cloud_hypervisor)
vmm: bin/vmm-sandboxer bin/kuasar.img bin/vmlinux.bin
else
# stratovirt or qemu
vmm: bin/vmm-sandboxer bin/kuasar.initrd bin/vmlinux.bin
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

ifeq ($(HYPERVISOR), cloud_hypervisor)
	@install -p -m 640 bin/kuasar.img ${DEST_DIR}${INSTALL_DIR}/kuasar.img
	@install -p -m 640 vmm/sandbox/config_clh.toml ${DEST_DIR}${INSTALL_DIR}/config.toml
else
# stratovirt or qemu
	@install -p -m 640 bin/kuasar.initrd ${DEST_DIR}${INSTALL_DIR}/kuasar.initrd
	@install -p -m 640 vmm/sandbox/config_${HYPERVISOR}_${ARCH}.toml ${DEST_DIR}${INSTALL_DIR}/config.toml
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

# E2E Testing targets
test-e2e: ## Run full e2e integration tests (requires environment setup)
	@$(MAKE) -f Makefile.e2e test-e2e

test-e2e-framework: ## Run e2e framework unit tests (no service startup required)
	@$(MAKE) -f Makefile.e2e test-e2e-framework

test-e2e-runc: ## Test only runc runtime
	@$(MAKE) -f Makefile.e2e test-e2e-runc

test-e2e-parallel: ## Run all tests in parallel
	@$(MAKE) -f Makefile.e2e test-e2e-parallel

setup-e2e-env: ## Setup complete e2e testing environment
	@$(MAKE) -f Makefile.e2e setup-e2e-env

build-runc-deps: ## Build runc runtime dependencies for e2e testing
	@$(MAKE) -f Makefile.e2e build-runc-deps

verify-e2e: ## Verify e2e test environment
	@hack/verify-e2e.sh

local-up: ## Start local Kuasar cluster for testing
	@hack/local-up-kuasar.sh

clean-e2e: ## Clean e2e test artifacts
	@$(MAKE) -f Makefile.e2e clean-e2e

.PHONY: help
help: ## Display this help screen
	@echo "Kuasar Build System"
	@echo ""
	@echo "Available targets:"
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z0-9_-]+:.*##/ { printf "  %-20s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo ""
	@echo "Variables:"
	@echo "  HYPERVISOR       Hypervisor to use (default: cloud_hypervisor)"
	@echo "  GUESTOS_IMAGE    Guest OS image type (default: centos)"
	@echo "  WASM_RUNTIME     WebAssembly runtime (default: wasmedge)"
	@echo "  KERNEL_VERSION   Kernel version (default: 6.12.8)"
	@echo "  ARCH             Target architecture (default: x86_64)"
	@echo "  ENABLE_YOUKI     Enable youki features (default: false)"
