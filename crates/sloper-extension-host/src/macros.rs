macro_rules! cfg_runtime {
    ($($item:item)*) => { $(
        #[cfg(feature = "runtime")]
        #[cfg_attr(docsrs, doc(cfg(feature = "runtime")))]
        $item
    )* };
}
