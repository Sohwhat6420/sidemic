//! JNI entry points for `com.truephonelink.server.NativeVoice`. The phone server System.load()s this library
//! from /data/local/tmp and hands it one 480-sample block every 10 ms.

use std::panic::{catch_unwind, AssertUnwindSafe};

use jni::objects::{JClass, JFloatArray};
use jni::sys::{jfloat, jint, jlong};
use jni::JNIEnv;
use tpl_voice::{Chain, NsKind, Params, BLOCK};

struct Handle {
    chain: Chain,
    buf: Vec<f32>,
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_truephonelink_server_NativeVoice_create(
    mut env: JNIEnv,
    _class: JClass,
    denoise: jint,
    atten_db: jfloat,
    target_db: jfloat,
    max_gain_db: jfloat,
    expand_db: jfloat,
    ceiling_db: jfloat,
    presence_db: jfloat,
) -> jlong {
    let p = Params {
        ns: match denoise {
            1 => NsKind::Rnnoise,
            2 => NsKind::Dfn,
            _ => NsKind::None,
        },
        ns_atten_db: atten_db,
        target_db,
        max_gain_db,
        expand_db,
        ceiling_db,
        presence_db,
        ..Params::default()
    };
    match catch_unwind(|| Chain::new(&p)) {
        Ok(Ok(chain)) => Box::into_raw(Box::new(Handle { chain, buf: vec![0.0; BLOCK] })) as jlong,
        Ok(Err(e)) => {
            let _ = env.throw_new("java/lang/IllegalStateException", e);
            0
        }
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalStateException", "voice chain panicked while loading");
            0
        }
    }
}

/// Processes `block` (exactly 480 floats) in place. On any failure the block is left untouched, so the
/// caller still sends audio: the mic must never go silent because of the enhancer.
#[no_mangle]
pub extern "system" fn Java_com_truephonelink_server_NativeVoice_process(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    block: JFloatArray,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: the handle came from create() and Java calls destroy() exactly once, after the last process().
    let h = unsafe { &mut *(handle as *mut Handle) };
    if env.get_float_array_region(&block, 0, &mut h.buf).is_err() {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| h.chain.process(&mut h.buf))).is_ok() {
        let _ = env.set_float_array_region(&block, 0, &h.buf);
    }
}

#[no_mangle]
pub extern "system" fn Java_com_truephonelink_server_NativeVoice_latencySamples(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    if handle == 0 {
        return 0;
    }
    // SAFETY: as in process().
    let h = unsafe { &*(handle as *const Handle) };
    h.chain.latency_samples() as jint
}

#[no_mangle]
pub extern "system" fn Java_com_truephonelink_server_NativeVoice_destroy(_env: JNIEnv, _class: JClass, handle: jlong) {
    if handle != 0 {
        // SAFETY: as in process(); this is the single release.
        drop(unsafe { Box::from_raw(handle as *mut Handle) });
    }
}
