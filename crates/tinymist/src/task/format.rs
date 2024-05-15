use std::iter::zip;

use lsp_types::request::Formatting;
use lsp_types::TextEdit;
use tinymist_query::{typst_to_lsp, PositionEncoding};
use typst::syntax::Source;

use crate::server::ResponseResult;
use crate::FormatterMode;

pub async fn format(
    src: Source,
    mode: FormatterMode,
    width: usize,
    position_encoding: PositionEncoding,
) -> ResponseResult<Formatting> {
    match mode {
        FormatterMode::Typstyle => {
            let res = typstyle_core::Typstyle::new_with_src(src.clone(), width).pretty_print();
            Ok(calc_diff(src, res, position_encoding))
        }
        FormatterMode::Typstfmt => {
            let config = typstfmt_lib::Config {
                max_line_length: width,
                ..typstfmt_lib::Config::default()
            };
            let res = typstfmt_lib::format(src.text(), config);
            Ok(calc_diff(src, res, position_encoding))
        }
        FormatterMode::Disable => Ok(None),
    }
}

/// A simple implementation of the diffing algorithm, borrowed from
/// [`Source::replace`].
fn calc_diff(prev: Source, next: String, encoding: PositionEncoding) -> Option<Vec<TextEdit>> {
    let old = prev.text();
    let new = &next;

    let mut prefix = zip(old.bytes(), new.bytes())
        .take_while(|(x, y)| x == y)
        .count();

    if prefix == old.len() && prefix == new.len() {
        return Some(vec![]);
    }

    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let mut suffix = zip(old[prefix..].bytes().rev(), new[prefix..].bytes().rev())
        .take_while(|(x, y)| x == y)
        .count();

    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix += 1;
    }

    let replace = prefix..old.len() - suffix;
    let with = &new[prefix..new.len() - suffix];

    let range = typst_to_lsp::range(replace, &prev, encoding);

    Some(vec![TextEdit {
        new_text: with.to_owned(),
        range,
    }])
}
