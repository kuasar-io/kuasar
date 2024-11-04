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

    /// Config file path, for cloud hypervisor, qemu and stratovirt, default is `/var/lib/kuasar/config.toml`
    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "/var/lib/kuasar/config.toml"
    )]
    pub config: String,

    /// Sandboxer working directory, default is `/run/kuasar-vmm`
    #[arg(short, long, value_name = "DIR", default_value = "/run/kuasar-vmm")]
    pub dir: String,

    /// Address for sandboxer's server, default is `/run/vmm-sandboxer.sock`
    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "/run/vmm-sandboxer.sock"
    )]
    pub listen: String,

    // log_level is optional and should not have default value if not given, since
    // it can be defined in configuration file.
    /// Logging level for sandboxer [trace, debug, info, warn, error, fatal, panic]
    #[arg(long, value_name = "STRING")]
    pub log_level: Option<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::args::Args;

    #[test]
    fn test_args_parse_default() {
        let args = Args::parse();
        assert!(!args.version);
        assert_eq!(args.config, "/var/lib/kuasar/config.toml");
        assert_eq!(args.dir, "/run/kuasar-vmm");
        assert_eq!(args.listen, "/run/vmm-sandboxer.sock");
        assert!(args.log_level.is_none());
    }
}
