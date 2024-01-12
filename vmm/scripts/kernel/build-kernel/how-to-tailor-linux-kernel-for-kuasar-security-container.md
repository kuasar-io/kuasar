This document describes the tailoring and building methods of the linux kernel for the kuasar security container in different scenarios. Developers can quickly and automatically build a linux kernel image which adapts to the kuasar security container  by using the provided `build-kernel.sh` script.

## Tailoring Method

Linux kernel tailoring is a process of removing or retaining some kernel features or modules based on actual application scenario requirements to achieve the purposes of optimizing system performance, reducing memory usage, and improving security. 
Generally, there are two methods to tailor the linux kernel:

1.  **Subtraction Tailoring**: Disable the configuration options of unnecessary features based on the default kernel configuration.
2.  **Addition Tailoring**: Developers know which kernel capabilities are needed, and combine the various kernel feature configuration options from scratch.

The two tailoring methods have their own advantages and disadvantages:

 *  **The advantage of the "subtraction" tailoring method is that it is simple and convenient**, and can quickly tailor the kernel to meet functional requirements. However, the disadvantage is that every time the kernel version is updated, **the manual tailoring process needs to be repeated, and the tailoring process cannot be automated and inherited**.
 *  **The advantage of the "addition" tailoring mode is that the kernel can quickly and automatically tailored for different versions of the kernel**, and the memory footprint of the tailored kernel is very small. **The disadvantage is that it is difficult to get started**. Developers need to be familiar with the features of the kernel and know how to divide the kernel configuration options according to the kernel features. **This may require a significant amount of time for the first tailoring.**

After analyzing the advantages and disadvantages of the two tailoring methods mentioned above, and considering the requirements of supporting multiple versions of linux kernel, minimal kernel memory overhead requirement, and easy expansion of kernel tailoring configuration in the usage scenario of kuasar vmm sandbox, **the "addition" tailoring method is the most suitable choice**.

## Analysis of Kernel Capabilities Required by Kuasar Security Container 

Currently, security container is currently mainly used in serverless computing and the hybrid deployment of trusted and untrusted containers. The different characteristics of these two scenarios also have different requirements for the kernel capabilities of security container.

 - **Serverless computing scenario**
	The characteristics of this scenario are that the functions provided by the application are very simple, which are mainly focused on computation, sensitive to delay, have short running time, and require high density of single-machine deployment.
	 **These characteristics require the kernel to meet basic computing and network communication capabilities, and the kernel's memory overhead must be small enough.**
  - **Trusted and untrusted applications deployed together**
	  The characteristic of this scenario is that the applications are typically standard linux monolithic applications with complex functionality and high performance requirements, such as multi-tenant AI training/inference scenarios. To reduce performance loss, accelerator hardware devices need to be directly passthrough to secure containers. In addition, the device driver module can be loaded and complex networking modes can be supported. 
	  **The requirements for the kernel in these scenarios are advanced capabilities, including support for hardware device pass-through, multiple network modes, and loadable kernel modules.**
 
Based on the directory structure of kernel features output by the `make menuconfig` command and combined with the typical scenarios of security container, the kernel features can be divided into the following categories:
 *  Basic general configuration (without architecture differentiation)
 *  CPU architecture configuration
 *  Firmware configuration
 *  CPU
 *  Memory management
 *  ACPI driver
 *  PCI bus
 *  Hotplug/Unhotplug
 *  Kernel module
 *  Scheduling
 *  Storage management
     *  Block storage
     *  NVDIMM non-volatile memory
     *  SCSI protocol
 *  File system
     *  Ext4/Xfs basic file system
     *  9p/Virtio-fs shared file system
     *  NFS file system
     *  Fuse file system
 *  Network management
     *  IPv4/IPv6 support
     *  VLAN network
     *  Netfilter packet filtering
 *  Cgroup resource control management
 *  NameSpace isolation
 *  Security features
 *  Device drivers
     *  Character device driver
     *  Virtio device driver
 *  Debug capability

## Security Container Guest Kernel Classification

We abstract the kernel capabilities corresponding to the preceding two typical scenarios of security container. The kernel used by security container are classified into the following types:

 *  **micro-kernel**: The lightweight kernel adopts the MMIO bus structure with minimal memory overhead, and is used in conjunction with lightweight virtual machine mode (such as StratoVirt hypervisor's microVM virtual machine mode, Cloud-Hypervisor/Firecracker light-weight virtualization engine), making it suitable for serverless computing scenarios.
 *  **mini-kernel**: The kernel is miniaturized and adopts a PCI bus structure, providing advanced kernel functions such as ACPI/SCSI/NFS/kernel module loading, with rich features. The mini-kernel has rich functions and is combined with the standard VM mode. (e.g. standard VM mode for StratoVirt and Qemu) It is applicable to complex scenarios, such as trusted applications and untrusted applications deployed in the same machine.**

## Kernel Tailoring and Building Guide

### Description of Tailoring Command Parameters

```
$ ./build-kernel.sh --help
Usage: ./build-kernel.sh [options]
    --help, -h             print the usage
    --arch                 specify the hardware architecture: aarch64/x86_64
    --kernel-type          specify the target kernel type: micro/mini
    --kernel-dir           specify the kernel source directory
    --kernel-conf-dir      specify the kernel tailor conf directory
```

> **Note**
> 
>  *  `--arch`: specifies the target platform architecture of the kernel. The valid value can be aarch64 or x86_64.
>  *  `--kernel-type`: specifies the guest kernel type of the security container to be built. The valid value can be micro (for serverless computing) or mini (for trusted and untrusted applications).
>  *  `--kernel-dir`: specifies the absolute path of the kernel source code directory.
>  *  `--kernel-conf-dir`: specifies the absolute path of the directory where the kernel tailoring configuration files are stored.

### Tailoring and Building Kernels 

The following is an example for tailoring and building the mini type guest kernel in the aarch64 architecture:

```
$ ./build-kernel.sh --arch aarch64 --kernel-type mini --kernel-dir /home/test/kernel/linux-5.10/ --kernel-conf-dir /home/test/kuasar/vmm/scripts/kernel/build-kernel

Kernel Type: mini
Kernel Dir: /home/jpf/kernel/linux-5.10/
Kernel Conf Dir: /home/jpf/kuasar/vmm/scripts/kernel/build-kernel
Merge kernel fragments with /home/jpf/kuasar/vmm/scripts/kernel/build-kernel/small-kernel-aarch64.list successfully.
  SYNC    include/config/auto.conf.cmd
  CC      scripts/mod/devicetable-offsets.s
  HOSTCC  scripts/mod/modpost.o
  ......
  AR      init/built-in.a
  LD      vmlinux.o
  MODPOST vmlinux.symvers
  MODINFO modules.builtin.modinfo
  GEN     modules.builtin
  LD      .tmp_vmlinux.kallsyms1
  KSYMS   .tmp_vmlinux.kallsyms1.S
  AS      .tmp_vmlinux.kallsyms1.S
  LD      .tmp_vmlinux.kallsyms2
  KSYMS   .tmp_vmlinux.kallsyms2.S
  AS      .tmp_vmlinux.kallsyms2.S
  LD      vmlinux
  SORTTAB vmlinux
  SYSMAP  System.map
  MODPOST modules-only.symvers
  GEN     Module.symvers
  OBJCOPY arch/arm64/boot/Image
  GZIP    arch/arm64/boot/Image.gz
Build kernel successfully.
```

> Note:
>  When the version of the kernel was changed by user or some customized patches applied for the kernel, the dependency relationships of some CONFIG configuration options in  the kernel may change. When automatically generating the config configuration for the tailored kernel, an error message similar to **"CONFIG_XXX not in final .config"** may appear., it means that the kernel configuration item that needs to be enabled in the kernel fragment file is not present in the final kernel configuration file `.config`.

>  The reason for this error message is that the dependency relationships of the kernel configuration options in the tailored kernel fragment file list have changed. This may occur when the kernel version undergoes significant changes and the configuration dependency of the original `CONFIG_XXX` changes, or it may be due to existing problems with the dependency relationships of the kernel configuration options in the original kernel fragment file.
> 
> **Solution:**
> 
> 1.  In the directory of the built kernel source code, use the configuration GUI of `make menuconfig` to locate the problematic `CONFIG_XXX` configuration option, and check its dependency relationship with other kernel configuration options.
> 2.  Based on the kernel configuration dependency relationship found in Step 1, adjust the kernel configuration options in the kernel fragment file (which may involve adding new kernel configuration options or deleting some existing ones).

### Customizing Tailored Kernel Configuration Options ###

The core workflow of the `build-kernel.sh` script is as follows:
1.  Based on the input target architecture and kernel type information, the script locates the corresponding tailored configuration file in the kernel tailored configuration  directory. The rule for matching the tailored configuration files is `<kernel-type>-kernel-<arch>.list`.
2.  The script merges all kernel configuration option fragments stored in the tailored configuration file using the `scripts/kconfig/merge_config.sh` script which stored in the kernel source directory, and generates the final `.config` file for kernel building.
3.  Executing the kernel building process, generating the kernel binary image.

There are two ways for developers to customize some of the kernel tailoring configuration options:
 *  After the `build-kernel.sh` script automatically generates the merged kernel tailored configuration file `.config`, manually adjust it through the `make menuconfig` configuration GUI.
 *  Directly modify the kernel tailored configuration file `<kernel-type>-kernel-<arch>.list` which stored in the `kuasar/vmm/scripts/kernel/build-kernel/` directory and add or remove kernel fragments as needed.
 
The format of the contents in the kernel tailoring configuration file `<kernel-type>-kernel-<arch>.list` is: 

```
fragments/aarch64.conf
fragments/base.conf
fragments/block.conf
fragments/cgroup.conf
fragments/character.conf
fragments/cpu.conf
fragments/device.conf
fragments/filesystem.conf
fragments/mem.conf
fragments/namespace.conf
fragments/net.conf
fragments/security.conf
fragments/virtio.conf
```

Each line in the file represents a kernel fragment file that needs to be included, and all kernel configuration options in that fragment file will be added to the final generated kernel configuration file `.config`. For example, the contents of the kernel fragment file `fragments/cgroup.conf` are as follows:

```
CONFIG_CGROUPS=y
CONFIG_PAGE_COUNTER=y
CONFIG_MEMCG=y
CONFIG_MEMCG_SWAP=y
CONFIG_MEMCG_KMEM=y
CONFIG_BLK_CGROUP=y
CONFIG_CGROUP_WRITEBACK=y
CONFIG_CGROUP_SCHED=y
CONFIG_FAIR_GROUP_SCHED=y
CONFIG_CFS_BANDWIDTH=y
CONFIG_CGROUP_PIDS=y
CONFIG_CGROUP_FREEZER=y
CONFIG_CPUSETS=y
CONFIG_PROC_PID_CPUSET=y
CONFIG_CGROUP_DEVICE=y
CONFIG_CGROUP_CPUACCT=y
CONFIG_SOCK_CGROUP_DATA=y
```

## Tailored Kernel Memory Footprint Test ##

### Testing Method

Start a new StratoVirt microVM type lightweight virtual machine sandbox instance through `crictl runp` command and observe various indicator data.

Measurement method for various test indicators:
 *  **Kernel image file size**: Obtain directly by `ls -ahl <kernel-image-filename>` command.
 *  **Kernel memory overhead**: The total physical memory used by the Guest OS (which can be acquired by  `pmem -p <vm pid>` command ) **subtracts** the RSS memory overhead of the init process in the Guest OS.
 *  **Kernel cold start time**: Check the time consumed by the guest kernel to start the user-mode init process by `dmesg` command executed in the Guest OS.

### Test Results

| Test Type                   | Kernel image file size (MB) | Kernel memory overhead (MB) | Kernel Cold Start Time (ms) |
| --------------------------- | --------------------------- | ----------------------------------- | ------------------------- |
| kuasar-aarch64-micro-kernel | *7.7*                       | 46.2                                | *48.3*                    |
| kuasar-aarch64-mimi-kernel  | 11                          | *43.2*                              | 67.4                      |
| kata-aarch64-kernel         | 12                          | 45.4                                | 73.1                      |
| kuasar-x86_64-micro-kernel  | *3.5*                       | *60.2*                              | *63.2*                    |
| kuasar-x86_64-mini-kernel   | 5                           | 107.4                               | 106.9                     |
| kata-x86_64-kernel          | 5.8                         | 85                                  | 120.4                     |

- In the **aarch64 architecture**, Kuasar has a **34% decrease** in kernel cold-start time compared to the guest kernel tailored by Kata-Containers, and the kernel memory overhead remains basically the same.
- In the **x86_64 architecture**, Kuasar has a **47.5% decrease** in kernel cold-start time compared to the guest kernel tailored by Kata, and **29% decrease** in memory baseline overhead.