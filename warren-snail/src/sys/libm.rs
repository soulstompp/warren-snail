//! The C library's functions of a double that PostgreSQL calls, as the machine's libm defines them,
//! each behind a safe call: a float's function answers as PostgreSQL's does only when it is the same
//! function, and the toolchain links copies of its own under some of those names.

extern "C" {
    fn sin(x: f64) -> f64;
    fn cos(x: f64) -> f64;
    fn tan(x: f64) -> f64;
    fn sinh(x: f64) -> f64;
    fn cosh(x: f64) -> f64;
    fn atanh(x: f64) -> f64;
    fn exp(x: f64) -> f64;
    fn cbrt(x: f64) -> f64;
    fn pow(x: f64, y: f64) -> f64;
    fn erf(x: f64) -> f64;
    fn erfc(x: f64) -> f64;
    fn tgamma(x: f64) -> f64;
    fn lgamma(x: f64) -> f64;
}

type F1 = unsafe extern "C" fn(f64) -> f64;
type F2 = unsafe extern "C" fn(f64, f64) -> f64;

/// One of libm's functions of one double, which reads only its argument.
#[derive(Clone, Copy)]
pub(crate) struct Fn1(F1);

/// One of libm's functions of two doubles, which reads only its arguments.
#[derive(Clone, Copy)]
pub(crate) struct Fn2(F2);

impl Fn1 {
    pub(crate) fn call(self, x: f64) -> f64 {
        // SAFETY: the function takes a double and returns one, reading only its argument.
        unsafe { (self.0)(x) }
    }
}

impl Fn2 {
    pub(crate) fn call(self, x: f64, y: f64) -> f64 {
        // SAFETY: the function takes two doubles and returns one, reading only its arguments.
        unsafe { (self.0)(x, y) }
    }
}

/// libm's functions, looked up in the machine's libm.so.6 by name; where it has none, the copies
/// linked.
pub(crate) struct Libm {
    pub(crate) sin: Fn1,
    pub(crate) cos: Fn1,
    pub(crate) tan: Fn1,
    pub(crate) sinh: Fn1,
    pub(crate) cosh: Fn1,
    pub(crate) atanh: Fn1,
    pub(crate) exp: Fn1,
    pub(crate) cbrt: Fn1,
    pub(crate) pow: Fn2,
    pub(crate) erf: Fn1,
    pub(crate) erfc: Fn1,
    pub(crate) tgamma: Fn1,
    pub(crate) lgamma: Fn1,
}

pub(crate) fn libm() -> &'static Libm {
    static LIBM: std::sync::OnceLock<Libm> = std::sync::OnceLock::new();
    LIBM.get_or_init(|| {
        // SAFETY: dlopen reads the NUL-terminated name; the library stays open for the program.
        let h = unsafe { libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        let found = |name: &std::ffi::CStr| -> *mut libc::c_void {
            if h.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: the handle is open and the name NUL-terminated.
            unsafe { libc::dlsym(h, name.as_ptr()) }
        };
        let f1 = |name: &std::ffi::CStr, linked: F1| -> Fn1 {
            let p = found(name);
            if p.is_null() {
                Fn1(linked)
            } else {
                // SAFETY: libm's function of that name takes a double and returns one.
                Fn1(unsafe { std::mem::transmute::<*mut libc::c_void, F1>(p) })
            }
        };
        let f2 = |name: &std::ffi::CStr, linked: F2| -> Fn2 {
            let p = found(name);
            if p.is_null() {
                Fn2(linked)
            } else {
                // SAFETY: libm's function of that name takes two doubles and returns one.
                Fn2(unsafe { std::mem::transmute::<*mut libc::c_void, F2>(p) })
            }
        };
        Libm {
            sin: f1(c"sin", sin),
            cos: f1(c"cos", cos),
            tan: f1(c"tan", tan),
            sinh: f1(c"sinh", sinh),
            cosh: f1(c"cosh", cosh),
            atanh: f1(c"atanh", atanh),
            exp: f1(c"exp", exp),
            cbrt: f1(c"cbrt", cbrt),
            pow: f2(c"pow", pow),
            erf: f1(c"erf", erf),
            erfc: f1(c"erfc", erfc),
            tgamma: f1(c"tgamma", tgamma),
            lgamma: f1(c"lgamma", lgamma),
        }
    })
}

#[cfg(test)]
mod tests {
    /// Doubles over every exponent, drawn from their bits, and as many from -1000 to 1000.
    fn walk(n: usize) -> Vec<f64> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut out: Vec<f64> = (0..n).map(|_| f64::from_bits(next())).collect();
        out.extend((0..n).map(|_| (next() >> 11) as f64 / (1u64 << 53) as f64 * 2000.0 - 1000.0));
        out.extend([
            0.0,
            -0.0,
            0.5,
            1.0,
            -1.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MIN_POSITIVE,
            f64::MAX,
        ]);
        out
    }

    fn same(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    /// The machine's libm's function of that name.
    fn glibc(name: &std::ffi::CStr) -> *mut libc::c_void {
        // SAFETY: dlopen and dlsym read NUL-terminated names; the library stays open.
        let p = unsafe {
            let h = libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            assert!(!h.is_null(), "the machine's libm.so.6");
            libc::dlsym(h, name.as_ptr())
        };
        assert!(!p.is_null(), "{name:?} in libm.so.6");
        p
    }

    /// A function of libm's by its name, and std's method the snail calls in its place.
    type Named = (&'static std::ffi::CStr, fn(f64) -> f64);

    #[test]
    fn std_answers_as_glibc_where_the_snail_calls_std() {
        // The functions the snail calls through std's methods, none read with errno after it: each
        // must be the machine's libm's to the bit.
        let ones: [Named; 8] = [
            (c"asin", f64::asin),
            (c"acos", f64::acos),
            (c"atan", f64::atan),
            (c"tanh", f64::tanh),
            (c"asinh", f64::asinh),
            (c"acosh", f64::acosh),
            (c"log", f64::ln),
            (c"log10", f64::log10),
        ];
        let xs = walk(1 << 16);
        for (name, std) in ones {
            // SAFETY: libm's function of that name takes a double and returns one.
            let f = unsafe {
                std::mem::transmute::<*mut libc::c_void, super::F1>(glibc(name))
            };
            for &x in &xs {
                // SAFETY: as above; it reads only its argument.
                let g = unsafe { f(x) };
                assert!(same(g, std(x)), "{name:?}({x:e}): glibc {g:e}, std {:e}", std(x));
            }
        }
        // SAFETY: libm's atan2 takes two doubles and returns one.
        let atan2 = unsafe { std::mem::transmute::<*mut libc::c_void, super::F2>(glibc(c"atan2")) };
        let half = xs.len() / 2;
        for i in 0..half {
            let (y, x) = (xs[i], xs[i + half]);
            // SAFETY: as above; it reads only its arguments.
            assert!(same(unsafe { atan2(y, x) }, y.atan2(x)), "atan2({y:e}, {x:e})");
        }
    }
}
