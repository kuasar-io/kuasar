# How to run a Llama-3-8B inference application in Kubernetes?

## What is LlamaEdge?

The [LlamaEdge](https://github.com/LlamaEdge/LlamaEdge) project makes it easy for you to run LLM inference apps and 
create OpenAI-compatible API services for the Llama3 series of LLMs locally.

With WasmEdge, you can create and deploy very fast and very lightweight LLM inference applications, see
details in: https://www.secondstate.io/articles/wasm-runtime-agi/.

## How to run a llm inference application in Kuasar?

Since Kuasar v0.8.0, Kuasar wasm-sandboxer with `wasmedge` and `wasmedge_wasi_nn` 
features allows your WasmEdge application use the ability of WASI API for
performing Machine Learning inference: https://github.com/WebAssembly/wasi-nn.

This article is inspired by [Getting Started with Llama-3-8B](https://www.secondstate.io/articles/llama-3-8b/),
which introducing how to create an OpenAI-compatible API service for Llama-3-8B.

### Prerequisites

+ Install WasmEdge and plugins: 
`curl -sSf https://raw.githubusercontent.com/WasmEdge/WasmEdge/master/utils/install.sh | bash -s -- -v 0.13.5 --plugins wasi_logging wasi_nn-ggml`


### 1. Build docker image

We already have an example docker image on dockerhub: `docker.io/kuasario/llama-api-server:v1`.
Follow this if you want to build your own docker image with the llm applications, model and other requires.

+ Download the Llama-3-8B model GGUF file: Since the size of the model is 5.73 GB，it could take a while to download.
`curl -LO https://huggingface.co/second-state/Llama-3-8B-Instruct-GGUF/resolve/main/Meta-Llama-3-8B-Instruct-Q5_K_M.gguf`.

+ Get your LlamaEdge app: Take the api-server as example, download it by 
`curl -LO https://github.com/LlamaEdge/LlamaEdge/releases/latest/download/llama-api-server.wasm`. 
It is a web server providing an OpenAI-compatible API service, as well as an optional web UI, for llama3 models.

+ Download the chatbot web UI to interact with the model with a chatbot UI: 
```bash
  curl -LO https://github.com/LlamaEdge/chatbot-ui/releases/latest/download/chatbot-ui.tar.gz
  tar xzf chatbot-ui.tar.gz
  rm chatbot-ui.tar.gz
```

+ Build it! Here is an example DOCKERFILE:
```dockerfile
FROM scratch
COPY . /
CMD ["llama-api-server.wasm", "--prompt-template", "llama-3-chat", "--ctx-size", "4096", "--model-name", "Llama-3-8B", "--log-all"]
```
Build it with `docker build -t docker.io/kuasario/llama-api-server:v1 .`

### 2. Build and run Kuasar Wasm Sandboxer

```bash
git clone https://github.com/kuasar-io/kuasar.git
cd kuasar/wasm
cargo run --features="wasmedge, wasmedge_wasi_nn" -- --listen /run/wasm-sandboxer.sock --dir /run/kuasar-wasm
```

### 3. Config and containerd
Add the following sandboxer config in the containerd config file `/etc/containerd/config.toml`
```toml
[proxy_plugins]
  [proxy_plugins.wasm]
    type = "sandbox"
    address = "/run/wasm-sandboxer.sock"

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-wasm]
  runtime_type = "io.containerd.kuasar-wasm.v1"
  sandboxer = "wasm"
```

### 4. Create Kuasar wasm runtime

Suppose we are in a kubernetes cluster, all the workloads are managed by kubernetes. So how to let container
engine(containerd) know which runtime the workload should run in?  

[Container Runtimes](https://kubernetes.io/docs/setup/production-environment/container-runtimes/) is designed for launching and
running containers in Kubernetes. Thus, you should create a new container runtime `kubectl apply -f kuasar-wasm-runtimeclass.yaml`.
```yaml
    apiVersion: node.k8s.io/v1
    handler: kuasar-wasm
    kind: RuntimeClass
    metadata:
      name: kuasar-wasm
```

OK, the container show know what is `kuasar-wasm` ruintime.

### 5. Deploy your llm workload

The last thing is to deploy the llm workload, you can use the docker image in the step 1.

Run `kubectl apply llama-deploy.yaml` 

Here is an example deploy.yaml
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: llama
  labels:
    app: llama
spec:
  replicas: 1
  selector:
    matchLabels:
      app: llama
  template:
    metadata:
      labels:
        app: llama
    spec:
      containers:
      - command:
        - llama-api-server.wasm
        args: ["--prompt-template", "llama-3-chat", "--ctx-size", "4096", "--model-name", "Llama-3-8B"]
        env:
        - name: io.kuasar.wasm.nn_preload
          value: default:GGML:AUTO:Meta-Llama-3-8B-Instruct-Q5_K_M.gguf
        image: docker.io/kuasario/llama-api-server:v1
        name: llama-api-server
      runtimeClassName: kuasar-wasm
```
Make sure the `runtimeClassName` is the right runtime created in the last step 4.

Please note that we define an env `io.kuasar.wasm.nn_preload`, which will tell kuasar what will be loaded in `wasi_nn`
plugin. Normally including the alias of model, the inference backend, the execution target and the model file.

## Extension: Try with Kubernetes Service

In Kubernetes, a [Service](https://kubernetes.io/docs/concepts/services-networking/service/) is a method for exposing a 
network application that is running as one or more Pods in your cluster.

You can create a ClusterIP Service or LoadBalancer Service or whatever you want, and access llm service from outer cluster.

We do not provide examples since it has nothing to do with Kuasar!
