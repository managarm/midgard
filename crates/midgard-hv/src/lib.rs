#[allow(unused_macros)]
macro_rules! backend {
    (
        pub struct $ty:ident;
        trait $tr:ident {
            $(
                fn $name:ident(&$($self_:ident)+ $(, $arg:ident: $argty:ty)*) -> $ret:ty;
            )*
        }
    ) => {
        trait $tr {
            $(fn $name(&$($self_)+ $(, $arg: $argty)*) -> $ret;)*
        }
        impl $ty {
            $(
                #[inline(always)]
                pub fn $name(&$($self_)+ $(, $arg: $argty)*) -> $ret {
                    <Self as $tr>::$name(self $(, $arg)*)
                }
            )*
        }
    };
}
