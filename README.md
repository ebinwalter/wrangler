# wrangler
**Wrangler** is an opinionated solution to the problem of compiling GLSL shaders
into SPIR-V from within a Rust build script.  It works by keeping a record of
when a shader file was last modified and recompiling whenever that changes, and
compiling any new shaders that appear in the search directory.

Being relatively small (just under 300 lines), Wrangler is straightforward to
use.  The following example demonstrates all you could need to know in order
to use it:

```rs
use wrangler::{self, ShaderKind};

let ins = wrangler::Instructions {
    // paths are relative to the crate root
    record_path: "assets/shaders/shader_record.dat",
    output_root: "assets/shaders/compiled",
    search_root: "assets/shaders/source",
    to_compile: vec![ShaderKind::Vertex, ShaderKind::Fragment],
    compilation_error_terminates: true,
};
wrangler::run(ins).unwrap();
```

# License
Licensed under the BSD 3-Clause license.
