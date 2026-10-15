package com.truephonelink.server;

/** One 10 ms mono block in, processed in place. Implemented natively (Rust, with DeepFilterNet) and in Java. */
interface VoiceChain extends AutoCloseable {
    void process(float[] block, int n);

    String describe();

    @Override
    void close();
}
