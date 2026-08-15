//! Numeric helpers for the CLI: format-argument parsing.

use sdr_fox_core::IqFormat;

/// A clap-friendly sample-format enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FormatArg {
    /// Unsigned 8-bit IQ.
    Cu8,
    /// Signed 8-bit IQ.
    Cs8,
    /// Signed 16-bit IQ.
    Cs16,
    /// Float IQ.
    Cf32,
}

impl From<FormatArg> for IqFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Cu8 => IqFormat::Cu8,
            FormatArg::Cs8 => IqFormat::Cs8,
            FormatArg::Cs16 => IqFormat::Cs16,
            FormatArg::Cf32 => IqFormat::Cf32,
        }
    }
}

impl FormatArg {
    /// Canonical raw-file extension for this sample representation.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Cu8 => "cu8",
            Self::Cs8 => "cs8",
            Self::Cs16 => "cs16",
            Self::Cf32 => "cf32",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FormatArg;

    #[test]
    fn format_extensions_are_unambiguous() {
        assert_eq!(FormatArg::Cu8.extension(), "cu8");
        assert_eq!(FormatArg::Cs8.extension(), "cs8");
        assert_eq!(FormatArg::Cs16.extension(), "cs16");
        assert_eq!(FormatArg::Cf32.extension(), "cf32");
    }
}
