/*
Copyright 2024 The Kuasar Authors.

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
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, about, long_about = None)]
pub struct Args {
    /// Version info
    #[arg(short, long)]
    pub version: bool,

    /// Config file path, only for cloud hypervisor and stratovirt
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<String>,

    /// Sandboxer working directory
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<String>,

    /// Address for sandboxer's server
    #[arg(short, long, value_name = "FILE")]
    pub listen: Option<String>,
}
