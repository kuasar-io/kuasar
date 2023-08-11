/*
Copyright 2022 The Kuasar Authors.

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

pub trait ToParams {
    fn to_params(&self) -> Vec<Param>;
}

#[derive(Debug)]
pub struct Param {
    pub name: String,
    pub properties: Vec<Property>,
}

#[derive(Debug)]
pub struct Property {
    pub key: String,
    pub value: String,
    pub ignore_key: bool,
}

impl Property {
    pub fn new() -> Self {
        Self {
            key: "".to_string(),
            value: "".to_string(),
            ignore_key: false,
        }
    }
}

// visit: https://rust-lang.github.io/rust-clippy/master/index.html#new_without_default
impl Default for Property {
    fn default() -> Self {
        Self::new()
    }
}

impl Param {
    pub fn new(name: &str) -> Param {
        Param {
            name: name.to_string(),
            properties: vec![],
        }
    }

    #[allow(dead_code)]
    pub fn get(&self, key: &str) -> Option<String> {
        self.properties
            .iter()
            .find(|&x| x.key == key)
            .map(|p| p.value.to_string())
    }
}

pub trait ToCmdLineParams {
    // hyphen is either "-" or "--"
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String>;
}

impl<T> ToCmdLineParams for T
where
    T: ToParams,
{
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let device_params = self.to_params();
        device_params
            .iter()
            .flat_map(|x| {
                let mut res = vec![];
                res.push(format!("{}{}", hyphen, x.name));
                let param_str = x
                    .properties
                    .iter()
                    .map(|p| {
                        if p.ignore_key {
                            p.value.to_string()
                        } else {
                            format!("{}={}", &p.key, &p.value)
                        }
                    })
                    .collect::<Vec<String>>()
                    .join(",");
                res.push(param_str);
                res
            })
            .collect()
    }
}

pub struct StringParam(String, String);

impl ToCmdLineParams for StringParam {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        vec![format!("{}{}", hyphen, self.0), self.1.to_string()]
    }
}

impl StringParam {
    pub fn new(key: &str, value: String) -> Self {
        Self(key.to_string(), value)
    }
}

pub struct BoolParam(String, bool);

impl ToCmdLineParams for BoolParam {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        if self.1 {
            vec![format!("{}{}", hyphen, self.0)]
        } else {
            vec![]
        }
    }
}

#[allow(dead_code)]
impl BoolParam {
    pub fn new(key: &str, value: bool) -> Self {
        Self(key.to_string(), value)
    }
}

pub struct VecParam<T>(String, Vec<T>);

#[allow(dead_code)]
impl<T> VecParam<T> {
    pub fn new(key: &str, val: Vec<T>) -> Self {
        Self(key.to_string(), val)
    }
}

impl<T> ToCmdLineParams for VecParam<T>
where
    T: ToCmdLineParams,
{
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let mut res = vec![];
        if self.1.is_empty() {
            return res;
        }
        for p in &self.1 {
            res.extend(p.to_cmdline_params(hyphen));
        }
        res
    }
}

impl ToCmdLineParams for VecParam<String> {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let mut res = vec![];
        if self.1.is_empty() {
            return res;
        }
        res.push(format!("{}{}", hyphen, self.0));
        res.push(
            self.1
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<String>>()
                .join(","),
        );
        res
    }
}

pub struct OptionParam<T>(String, Option<T>);

impl<T> OptionParam<T> {
    pub fn new(key: &str, val: Option<T>) -> Self {
        Self(key.to_string(), val)
    }
}

impl<T> ToCmdLineParams for OptionParam<T>
where
    T: ToCmdLineParams,
{
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let mut res = vec![];
        match &self.1 {
            None => {}
            Some(t) => {
                res.append(&mut t.to_cmdline_params(hyphen));
            }
        }
        res
    }
}

impl ToCmdLineParams for OptionParam<String> {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let mut res = vec![];
        match &self.1 {
            None => {}
            Some(v) => {
                res.push(format!("{}{}", hyphen, self.0));
                res.push(v.clone());
            }
        }
        res
    }
}
