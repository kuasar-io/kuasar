/*
Copyright 2023 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use qapi::qmp::QmpCommand;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryCpus {}

impl QmpCommand for QueryCpus {}
impl ::qapi_spec::Command for QueryCpus {
    const NAME: &'static str = "query-cpus";
    const ALLOW_OOB: bool = false;

    type Ok = Vec<CpuInfo>;
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CpuInfoArch {
    #[serde(rename = "x86")]
    X86,
    #[serde(rename = "Arm")]
    Arm,
}

impl ::core::str::FromStr for CpuInfoArch {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ::qapi_spec::Enum::from_name(s).ok_or(())
    }
}

unsafe impl ::qapi_spec::Enum for CpuInfoArch {
    fn discriminant(&self) -> usize {
        *self as usize
    }

    const COUNT: usize = 2;
    const VARIANTS: &'static [Self] = &[CpuInfoArch::X86, CpuInfoArch::Arm];
    const NAMES: &'static [&'static str] = &["x86", "Arm"];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "arch")]
pub enum CpuInfo {
    #[serde(rename = "arm")]
    Arm {
        #[serde(flatten)]
        #[serde(rename = "base")]
        base: CpuInfoBase,
        #[serde(flatten)]
        #[serde(rename = "Arm")]
        arm: CpuInfoArm,
    },
    #[serde(rename = "x86")]
    X86 {
        #[serde(flatten)]
        #[serde(rename = "base")]
        base: CpuInfoBase,
        #[serde(flatten)]
        #[serde(rename = "x86")]
        x86: CpuInfoX86,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfoBase {
    #[serde(rename = "CPU")]
    pub cpu: i64,
    #[serde(rename = "current")]
    pub current: bool,
    #[serde(rename = "halted")]
    pub halted: bool,
    #[serde(rename = "qom_path")]
    pub qom_path: String,
    #[serde(rename = "thread_id")]
    pub thread_id: i64,
}

impl From<(CpuInfoArm, CpuInfoBase)> for CpuInfo {
    fn from(val: (CpuInfoArm, CpuInfoBase)) -> Self {
        Self::Arm {
            arm: val.0,
            base: val.1,
        }
    }
}

impl From<(CpuInfoX86, CpuInfoBase)> for CpuInfo {
    fn from(val: (CpuInfoX86, CpuInfoBase)) -> Self {
        Self::X86 {
            x86: val.0,
            base: val.1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CpuInfoArm {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CpuInfoX86 {}
