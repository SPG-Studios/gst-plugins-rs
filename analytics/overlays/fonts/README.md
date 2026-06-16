# Bundled label font

`DejaVuSans-subset.ttf` is the font used to render all overlay label text. It is
embedded into the plugin at build time (`include_bytes!`) and is the *only* font
the renderer uses, so label rendering is byte-for-byte identical on every
machine — a prerequisite for the golden-image tests.

It is a subset of DejaVu Sans restricted to printable ASCII plus Latin-1
Supplement (label text is ASCII; Latin-1 gives headroom for accented object
class names). Regenerated with:

```sh
pyftsubset /usr/share/fonts/TTF/DejaVuSans.ttf \
  --unicodes=U+0020-007E,U+00A0-00FF \
  --output-file=fonts/DejaVuSans-subset.ttf \
  --no-hinting --desubroutinize --name-IDs='*' --recalc-bounds
```

`LICENSE-DejaVu` is the DejaVu / Bitstream Vera license, which permits
redistribution and embedding.
