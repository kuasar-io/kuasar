How to apply patch of two new features - File-Backend MMAP and External UFFD for cloud-hypervisor:

Supplement Notes:
===================
Base commit:
297b683fcb6c9a9b225d04213dc04b0faf8f9715

Patch:
filebackend-MMAP_and_external-UFFD_feature.patch

Path: <cloud-hypervisor-repo>/vmm/cloud-hypervisor_TwoNewFeatures_patch/filebackend-MMAP_and_external-UFFD_feature.patch

Build:
cargo build --release
cargo build --release --example external_uffd_handler
======================

Steps:
=======
git clone <cloud-hypervisor-repo>
cd cloud-hypervisor
git reset --hard 297b683fcb6c9a9b225d04213dc04b0faf8f9715
git apply <path-to-kuasar-repo>/vmm/cloud-hypervisor_TwoNewFeatures_patch/filebackend-MMAP_and_external-UFFD_feature.patch


cargo build --release
cargo build --release --example external_uffd_handler

