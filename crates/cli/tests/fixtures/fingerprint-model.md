The hexadecimal ONNX fixture in `fingerprint-model.hex` exercises real CPU
inference without downloading a production embedding model. Its IR version is
8, and its default-domain opset is 13. The graph casts `input_ids` to float,
unsqueezes axis 2, and concatenates that result with itself on axis 2. It accepts
an unused `attention_mask`, matching the embedder's input contract. Its output
is `[batch, sequence, 2]`.

The integration test provisions the graph and a WordLevel tokenizer into an
isolated Hugging Face cache. `codesage index --full` creates the vectors and
their fingerprint; MCP search loads the same graph through the production
loader. The fixture tests fingerprint enforcement, not retrieval quality.
An empty cached sidecar prevents the unpinned loader from probing the hub for
external weights; this graph stores its initializer inline.

Run `cargo test -p codesage --test mcp_fingerprint_test`. Linux requires ONNX
Runtime API 24 or newer; CI installs the CPU package `onnxruntime==1.24.4`.
You can install that package locally or set `ORT_DYLIB_PATH` to a compatible
runtime library. Missing runtime dependencies fail the test rather than skip it.
