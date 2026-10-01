# LocalServer

HTTP/1.1 server in Rust (single thread, mio event loop).

    cargo run -- config.yaml
    cargo test

Owners: 

        A = network (server, connection, http/parser, http/serialize) @mennaas

        B = app (config, router, static_files, upload, error, utils) @ayazizi

        C = cgi @ranniz
