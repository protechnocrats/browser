#!/usr/bin/env python3
"""Deterministically (re)generates the committed fuzz seed corpus.

Seeds come from three places:
  * real fixtures (crates/images/tests/fixtures/*),
  * one small well-formed example per target, and
  * the hostile inputs found/fixed during the security pass — deep
    `:not()`, extreme nesting, WebP/PNG header bombs, oversized
    font-size, malformed cookies — so each fixed bug is also a permanent
    starting point for the fuzzer to mutate from.

Existing hand-written seeds (corpus/parse_document/0*.html) are left
alone. Run from the fuzz/ directory:  python3 generate_seeds.py
"""
import os
import shutil
import struct

ROOT = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(ROOT, "corpus")
FIXTURES = os.path.join(ROOT, "..", "crates", "images", "tests", "fixtures")


def write(target, name, data):
    d = os.path.join(CORPUS, target)
    os.makedirs(d, exist_ok=True)
    if isinstance(data, str):
        data = data.encode()
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


# ---- parse_document (adds to the six hand-written seeds) -------------
write("parse_document", "10_deep_nesting.html", "<div>" * 600 + "x" + "</div>" * 600)
write("parse_document", "11_deep_mismatched_closes.html", "<b><i>" * 400 + "</u></p>" * 400)
write("parse_document", "12_entities.html", "<p title='&lt;&#x41;&#65;&bogus;&amp;amp'>&nbsp;&#xFFFFFFFF;&#0;</p>")
write("parse_document", "13_unterminated.html", "<html><body><div class='a' id=<p attr=\"never closed")
# Milestone 40b: names the page invents (interned in the document's own
# table), next to well-known ones, upper-case ones, and a tag and an attribute
# name far longer than any well-known name.
write("parse_document", "14_invented_names.html",
      "<html><body>" + "".join(f'<x-el{i} data-a{i}="1" aria-z{i}>' for i in range(3000))
      + "<div class=a id=b DATA-UPPER=1 " + "n" * 100000 + "=v></div><" + "t" * 70000 + "></body></html>")

# ---- parse_stylesheet ------------------------------------------------
write("parse_stylesheet", "01_basic.css",
      "body{margin:0}.a>.b+.c~.d{color:#fff;background:rgb(1,2,3)}a:hover,li:nth-child(2n+1){x:y}")
write("parse_stylesheet", "02_deep_not.css", "a" + ":not(" * 400 + "b" + ")" * 400 + "{color:red}")
write("parse_stylesheet", "03_at_rules.css",
      "@media (min-width:600px) and (max-width:900px){p{color:red}}"
      "@keyframes k{from{opacity:0}50%{opacity:.5}to{opacity:1}}"
      "@font-face{font-family:x;src:url(a.woff2)}@import 'x.css';")
write("parse_stylesheet", "04_unterminated.css", "p{color:red;" + "q{" * 50 + "/* never closed")
write("parse_stylesheet", "05_oversized_values.css",
      "p{font-size:99999999999999px;width:1e308px;height:-1e308px;margin:calc(1px + 2 * (3px")
write("parse_stylesheet", "06_huge_selector_list.css", ",".join(f".c{i}" for i in range(2000)) + "{a:b}")
write("parse_stylesheet", "07_pseudo_variety.css",
      "a:nth-child(-n+3):not(.x):not(:first-child)::before{content:'\\0041'}[data-x^='a' i]{x:y}")

# ---- parse_inline_style ----------------------------------------------
write("parse_inline_style", "01_basic", "color: red; background-color: #00ff00; margin: 4px 8px")
write("parse_inline_style", "02_oversized", "font-size:99999999999999999999px;width:1e999px;opacity:1e-999")
write("parse_inline_style", "03_malformed", ";;;:::;color;:red;a:b:c;;{}}}{url(")
write("parse_inline_style", "04_transform_transition",
      "transform:rotate(45deg) skew(1deg,2deg) translate(1px,2px);transition:all 1s cubic-bezier(0,0,1,1)")

# ---- decode_auto -----------------------------------------------------
if os.path.isdir(FIXTURES):
    os.makedirs(os.path.join(CORPUS, "decode_auto"), exist_ok=True)
    for fn in sorted(os.listdir(FIXTURES)):
        shutil.copyfile(os.path.join(FIXTURES, fn), os.path.join(CORPUS, "decode_auto", fn))

# WebP "header bomb": VP8X chunk declaring a 2^24 x 2^24 canvas, no pixel data.
def webp_vp8x(w, h):
    canvas = struct.pack("<I", w - 1)[:3] + struct.pack("<I", h - 1)[:3]
    chunk = b"VP8X" + struct.pack("<I", 10) + b"\x00\x00\x00\x00" + canvas
    return b"RIFF" + struct.pack("<I", 4 + len(chunk)) + b"WEBP" + chunk

write("decode_auto", "webp_header_bomb.webp", webp_vp8x(1 << 24, 1 << 24))
# PNG IHDR declaring 2^31-1 x 2^31-1 with no IDAT.
ihdr = struct.pack(">IIBBBBB", 0x7FFFFFFF, 0x7FFFFFFF, 8, 6, 0, 0, 0)
write("decode_auto", "png_header_bomb.png",
      b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + ihdr + b"\x00\x00\x00\x00")
write("decode_auto", "truncated_jpeg.jpg", b"\xff\xd8\xff\xe0\x00\x10JFIF\x00")
write("decode_auto", "gif_zero_size.gif", b"GIF89a\x00\x00\x00\x00\x00\x00;")
write("decode_auto", "empty_and_junk", b"\x00")

# ---- url_origin ------------------------------------------------------
write("url_origin", "01_basic", "https://example.com/a/b?x=1#f\n../c.css")
write("url_origin", "02_userinfo_backslash", "http://user:pw@EXAMPLE.com:80\\@evil.com/\n\\\\evil.com\\x")
write("url_origin", "03_idn_ipv6", "http://[::1]:8080/\nhttp://xn--e1afmkfd.xn--p1ai/")
write("url_origin", "04_opaque", "data:text/html,<b>x\njavascript:alert(1)")
write("url_origin", "05_dots_and_ports", "http://a.b.:99999/\nhttps://example.com.:443/")

# ---- cookie_header ---------------------------------------------------
write("cookie_header", "01_basic", "session=abc; Path=/; Secure; HttpOnly; Max-Age=3600\nexample.com\n/")
write("cookie_header", "02_public_suffix_domain", "a=b; Domain=com\nexample.com\n/")
write("cookie_header", "03_malformed", "=;;;Domain=;Path=;Expires=garbage;Max-Age=-999999999999999999999\n\n")
write("cookie_header", "04_persisted_jar",
      "a\tb\texample.com\t/\t99999999999\t1\t0\nshort\tline\n#comment\n\ta\t\t\t\t\t\n")
write("cookie_header", "05_case_and_dot", "K=v; DOMAIN=.EXAMPLE.COM; PATH=/a/b\nwww.example.com\n/a/b/c")

# ---- layout_document (html NUL css) ----------------------------------
write("layout_document", "01_basic",
      "<html><body><div class=a>hello <b>world</b></div><p>para</p></body></html>\0"
      ".a{padding:4px;border:1px solid red}p{margin:8px}")
write("layout_document", "02_flex",
      "<html><body><div class=f><div class=i>a</div><div class=i>b</div></div></body></html>\0"
      ".f{display:flex;gap:4px;justify-content:space-between;width:200px}.i{flex:1 1 auto;min-width:min-content}")
write("layout_document", "03_deep_tree", "<html><body>" + "<div>" * 700 + "deep" + "</div>" * 700 + "</body></html>\0div{margin:1px}")
write("layout_document", "04_oversized_font",
      "<html><body><p>x</p></body></html>\0p{font-size:99999999px;width:99999999px;line-height:1e9px}")
write("layout_document", "05_images_and_forms",
      "<html><body><img src='missing.png' width=100000 height=100000><input type=text value='v'>"
      "<textarea>t</textarea><button>b</button></body></html>\0img{width:50%}input{padding:1e9px}")
write("layout_document", "06_transform_transition",
      "<html><body><div id=a>a</div></body></html>\0"
      "#a{transform:rotate(90deg);transition:transform 1s ease}@keyframes k{from{opacity:0}to{opacity:1}}#a{animation:k 1s}")
write("layout_document", "07_deep_not_and_media",
      "<html><body><p class=x>t</p></body></html>\0"
      "p" + ":not(" * 30 + ".y" + ")" * 30 + "{color:red}@media(max-width:10px){p{display:none}}")

# ---- webidl_parse (Milestone 40) ---------------------------------------
write("webidl_parse", "01_dom", open(os.path.join(os.path.dirname(__file__), "..", "crates", "script", "webidl", "dom.webidl")).read())
write("webidl_parse", "02_window", open(os.path.join(os.path.dirname(__file__), "..", "crates", "script", "webidl", "window.webidl")).read())
write("webidl_parse", "03_malformed", "interface A : { attribute; }; [Global interface; enum E {}; callback = ; namespace n { attribute long x; };")
write("webidl_parse", "04_nesting", "interface A { " + "Promise<" * 40 + "long" + ">" * 40 + " f(); };")

# ---- pattern_matches (pattern NEWLINE value) --------------------------
write("pattern_matches", "01_basic", "[A-Za-z]{3,}\nabc")
write("pattern_matches", "02_phone", "\\d{3}-\\d{4}\n555-1234")
write("pattern_matches", "03_redos_audit_case", "(a+)+$\n" + "a" * 28 + "!")
write("pattern_matches", "04_redos_alternation", "(a|aa)+$\n" + "a" * 64 + "!")
write("pattern_matches", "05_backreference", "(a)\\1\naa")
write("pattern_matches", "06_lookaround", "(?=x)x\n(?<!a)b")
write("pattern_matches", "07_invalid", "[unclosed\n(\n\\")
write("pattern_matches", "08_bombs", "(?:a{1000}){1000}\n((((((((((((((((((((a))))))))))))))))))))")
write("pattern_matches", "09_unicode", "\\p{L}+\n\u00e9\u4e2d\u6587")

# ---- psl_parse (list text NUL host) ------------------------------------
write("psl_parse", "01_real_shape", "// comment\ncom\nco.uk\n*.ck\n!www.ck\n\u516c\u53f8.cn\n\0www.example.co.uk")
write("psl_parse", "02_wildcards_and_exceptions", "*.a\n!b.a\n*.*.c\n!*.d\n\0x.b.a")
write("psl_parse", "03_hostile_lines", "!\n*.\n!*.\n\n\t\n\u202e\n" + "x." * 200 + "\n\0" + "y." * 200)
write("psl_parse", "04_no_separator", "com\nnet\norg\n")

# ---- origin_parse (url NL other-url NL allow-origin) -------------------
write("origin_parse", "01_same_origin_default_port", "https://example.com:443/a\nhttps://EXAMPLE.com/b\nhttps://example.com")
write("origin_parse", "02_userinfo_backslash", "https://good.com@evil.com/\nhttps://evil.com\\@good.com/\n*")
write("origin_parse", "03_idn_homograph", "https://\u0430pple.com/\nhttps://apple.com/\nnull")
write("origin_parse", "04_trailing_dot_ipv6", "http://example.com./\nhttp://[0:0::1]:8080/\nhttp://[::1]:8080")
write("origin_parse", "05_opaque_schemes", "data:text/html,x\nfile:///etc/passwd\nnull")
write("origin_parse", "06_blob", "blob:https://example.com/uuid\nblob:blob:https://example.com/x\nhttps://example.com")
write("origin_parse", "07_garbage", "\u0000\u202e\nhttp://%zz/\n\u0000")

# ---- font_parse (Milestone 32): truncations of the bundled font + bad headers
import os as _os, gzip as _gzip, zlib as _zlib
_font = open(_os.path.join(_os.path.dirname(__file__), "..", "crates", "text", "assets", "DejaVuSans.ttf"), "rb").read()
write("font_parse", "01_font_prefix_512", _font[:512])
write("font_parse", "02_font_prefix_4096", _font[:4096])
write("font_parse", "03_truetype_magic_only", b"\x00\x01\x00\x00" + b"\x00" * 60)
write("font_parse", "04_otto_huge_table_count", b"OTTO\xff\xff" + b"\x00" * 58)
write("font_parse", "05_woff_header", b"wOFF\x00\x01\x00\x00" + b"\xff" * 40)
write("font_parse", "06_empty", b"")

# ---- font_face_src
write("font_face_src", "01_typical", "local(\"Open Sans\"), url(/f/open.woff2) format(\"woff2\"), url('/f/open.woff') format('woff')")
write("font_face_src", "02_quoted_commas", "url('/a,b.woff'), url(\"c)d.ttf\") format(truetype) tech(variations)")
write("font_face_src", "03_malformed", "url(, format(, local(), url('x) ,,, ((( ")
write("font_face_src", "04_data_url", "url(data:font/woff2;base64,d09GMgABAAAAA) format(woff2)")

# ---- css_length (Milestone 33)
write("css_length", "01_units", "1.5em")
write("css_length", "02_calc", "calc(100% - 2 * (1rem + 3px))")
write("css_length", "03_clamp", "clamp(1rem, 2.5vw + 4px, 3rem)")
write("css_length", "04_deep", "calc(" * 40 + "1px" + ")" * 40)
write("css_length", "05_overflow", "calc(1e38px * 1e38)")
write("css_length", "06_bad", "calc(1px+2px) min(,) 1e999px -0.0e-0vmin")

# ---- css_color (Milestone 33)
write("css_color", "01_hex", "#1a2b3c80")
write("css_color", "02_rgb", "rgb(10% 20% 30% / .5)")
write("css_color", "03_hsl", "hsla(-720deg, 150%, 50%, 2)")
write("css_color", "04_named", "RebeccaPurple")
write("css_color", "05_bad", "rgb(1,,2) #ggg hsl(nan 1 1) currentColor")

# ---- css_background (Milestone 33)
write("css_background", "01_shorthand", "url(a.png) no-repeat right 10px bottom 5% / 20px auto rgb(1 2 3)")
write("css_background", "02_layers", "url(a.png), linear-gradient(red, blue), #fff")
write("css_background", "03_bad", "left left / / cover cover, , url(")
write("css_background", "04_huge", "1e38px 1e38px / 1e-38px")

# ---- css_grid (Milestone 33)
write("css_grid", "01_tracks", "[a] 100px repeat(3, minmax(10px, 1fr)) [b] auto fit-content(40%)")
write("css_grid", "02_auto_fill", "repeat(auto-fill, minmax(50px, 1fr))")
write("css_grid", "03_lines", "2 / span 3")
write("css_grid", "04_areas", "'a a b' 'c . b'")
write("css_grid", "05_bad", "repeat(99999999, 1fr) minmax(1fr, 1fr) -3 / 0 span -1")

# ---- content_decoding (first byte selects the encoding: 0 gzip, 2 deflate, 3 identity, 6 br)
_html = b"<html><body><p>hello</p></body></html>" * 20
write("content_decoding", "01_gzip", b"\x00" + _gzip.compress(_html))
write("content_decoding", "02_zlib_deflate", b"\x02" + _zlib.compress(_html))
_raw = _zlib.compressobj(wbits=-15); _rawd = _raw.compress(_html) + _raw.flush()
write("content_decoding", "03_raw_deflate", b"\x02" + _rawd)
write("content_decoding", "04_identity", b"\x03" + _html)
write("content_decoding", "05_small_bomb", b"\x00" + _gzip.compress(b"\x00" * (4 << 20)))
write("content_decoding", "06_gzip_with_name", b"\x00" + _gzip.compress(_html, mtime=0)[:3] + b"\x08" + _gzip.compress(_html, mtime=0)[4:10] + b"page.html\x00" + _gzip.compress(_html, mtime=0)[10:])
write("content_decoding", "07_unsupported", b"\x06" + b"\x0b\x02\x80")

# ---- http_headers ("Name: value" lines)
write("http_headers", "01_cacheable", "Cache-Control: max-age=3600, public\nETag: \"abc\"\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\nContent-Type: text/html; charset=utf-8")
write("http_headers", "02_no_store", "Cache-Control: no-store, no-cache, must-revalidate\nExpires: 0\nVary: *")
write("http_headers", "03_hostile_numbers", "Cache-Control: max-age=99999999999999999999999\nCache-Control: max-age=-1, s-maxage=1e999\nAge: 184467440737095516160")
write("http_headers", "04_content_types", "Content-Type: \nContent-Type: ;charset=\nContent-Type: text/html;;;charset=\"x\nContent-Type: text/html; charset=Shift_JIS")

# ---- js_script (DOM: div#a input#i form#f a#l p#p)
write("js_script", "01_dom_bindings", "var a = document.getElementById('a'); a.textContent = 'x'; a.style.backgroundColor = 'red'; var d = document.createElement('div'); d.setAttribute('id', 'n'); d.textContent = 'new'; a.appendChild(d); document.querySelector('#p').textContent = document.getElementById('n').textContent;")
write("js_script", "02_events", "document.getElementById('a').addEventListener('click', function (e) { e.stopPropagation(); e.preventDefault(); }, true); document.addEventListener('keydown', function (e) { throw new Error(e.key); }); document.getElementById('l').addEventListener('click', function (e) { e.preventDefault(); });")
write("js_script", "03_storage_raf_fetch", "localStorage.setItem('k', 'v'); sessionStorage.setItem('k', localStorage.getItem('k')); requestAnimationFrame(function (t) { requestAnimationFrame(function () {}); }); fetch('/api').then(function (r) { return r.text(); }).catch(function () {}); var x = new XMLHttpRequest(); x.open('GET', '/x'); x.send();")
write("js_script", "04_hostile", "var s = 'x'; for (var i = 0; i < 20; i++) { s = s + s; } try { localStorage.setItem('big', s); } catch (e) {} (function f(n) { return n ? f(n - 1) : 0; })(100000); document.getElementById(null); document.createElement(undefined).appendChild({__node_id: -1});")
write("js_script", "05_syntax_errors", "function ( { ]]]; @@@; \u0000; `unterminated")
# Milestone 39: the full tree mutation API, including moves that would make a cycle.
# Milestone 40 (ADR 023): wrappers carry their node in an internal slot;
# prototype members called on the wrong `this`, accessors torn off, a node id
# forged, wrappers compared and dropped in bulk.
write("js_script", "07_generated_bindings", "var a = document.getElementById('a'); var same = a === document.getElementById('a'); var proto = Object.getPrototypeOf(a); try { proto.appendChild.call(5, a); } catch (e) {} try { Object.getOwnPropertyDescriptor(Object.getPrototypeOf(proto), 'textContent').get.call({}); } catch (e) {} try { a.appendChild({__node_id: a.__node_id}); } catch (e) {} var x = new XMLHttpRequest(); x.onload = 5; x.onload = function () { x.onload = null; }; x.withCredentials = {}; for (var i = 0; i < 1000; i++) { document.createElement('i').style.color = i; } document.addEventListener('click', null); requestAnimationFrame(function () { a.textContent = same; }); fetch('/x', {credentials: 'bogus'}).catch(function () {}); console.log(a, proto, Symbol());")
write("js_script", "06_tree_mutations", "var a = document.getElementById('a'); var p = document.getElementById('p'); var t = document.createTextNode('t'); a.insertBefore(t, null); var d = document.createElement('div'); a.insertBefore(d, null); var c = a.cloneNode(true); p.appendChild(c); a.appendChild(p); p.appendChild(a); var r = a.removeChild(d); a.appendChild(r); a.replaceChild(document.createElement('i'), r); a.insertBefore({__node_id: 1e300}, {}); a.removeChild(null); a.replaceChild(null, null);")

# ---- ipc_decode (ADR 016: messages between the host and a renderer, as
# `engine::ipc` writes them — little-endian, u32-length strings)
import struct as _st
def _s(t):
    b = t.encode()
    return _st.pack("<I", len(b)) + b
_crashed = b"\x02" + _s("page loop panicked")
_started = b"\x04\x01" + _s("filesystem: closed; system calls: filtered")
_image = b"\x01" + _st.pack("<QII", 7, 2, 1) + _st.pack("<I", 8) + bytes(range(8))
_fetch = b"\x03" + _st.pack("<Q", 9) + _s("https://cdn.example/a.png") + b"\x01" + b"\x01" + _s("https://news.example/") + b"\x00" + b"\x00" + b"\x00" + b"\x02"
_frame = (_st.pack("<ff", 800.0, 600.0) + bytes([255, 255, 255, 255]) + _st.pack("<I", 1)
          + b"\x00" + _st.pack("<ffff", 1, 2, 3, 4) + bytes([1, 2, 3, 255]) + _st.pack("<f", 0)
          + _st.pack("<IIQ", 512, 64, 3) + _st.pack("<I", 0) + _st.pack("<I", 0)
          + b"\x00" + _st.pack("<Q", 1) + b"\x01" + _st.pack("<QQ", 0, 0) + b"\x00" + _st.pack("<QQ", 1, 0) + b"\x01"
          + _st.pack("<I", 17))  # Milestone 38b: animation_interval_ms
# Milestone 34: a frame with every structural item — a group, a rounded
# clip, a scroll layer — around a rect.
_rect = b"\x00" + _st.pack("<ffff", 1, 2, 3, 4) + bytes([1, 2, 3, 255]) + _st.pack("<f", 0)
_items = (b"\x08" + _st.pack("<fffff", 0.5, 0, 0, 10, 10)
          + b"\x04" + _st.pack("<fffff", 0, 0, 10, 10, 2)
          + b"\x06" + _st.pack("<" + "f" * 12, 1, 0, 0, 1, 0, -5, 0, 0, 10, 10, 0, 0) + b"\x01" + _st.pack("<Q", 12) + b"\x01" + _st.pack("<ffff", 0, 0, 10, 10)
          + _rect + b"\x07" + b"\x05" + b"\x09")
_frame2 = (_st.pack("<ff", 800.0, 600.0) + bytes([255, 255, 255, 255]) + _st.pack("<I", 7) + _items
          + _st.pack("<IIQ", 512, 64, 3) + _st.pack("<I", 0) + _st.pack("<I", 0)
          + b"\x00" + _st.pack("<Q", 0) + b"\x00" + _st.pack("<QQ", 1, 1) + b"\x00" + _st.pack("<QQ", 7, 0) + b"\x00"
          + _st.pack("<I", 133))  # Milestone 38b: a degraded frame
_published = b"\x00" + _st.pack("<I", 1) + b"\x01\x01" + _s("https://news.example/") + b"\x01" + _frame + b"\x01" + _st.pack("<qq", 16_000_000, 8_000_000)
_published2 = b"\x00" + _st.pack("<I", 1) + b"\x01\x01" + _s("https://news.example/") + b"\x01" + _frame2 + b"\x01" + _st.pack("<qq", 16_000_000, 8_000_000)
_tick = b"\x02" + _st.pack("<q", -5)
_reply = b"\x04" + _st.pack("<Q", 9) + b"\x00" + _st.pack("<I", 200) + _s("https://cdn.example/a.png") + _st.pack("<I", 1) + _s("content-type") + _s("image/png") + _st.pack("<I", 3) + b"abc"
write("ipc_decode", "01_crashed", _crashed)
write("ipc_decode", "02_started", _started)
write("ipc_decode", "03_image", _image)
write("ipc_decode", "04_fetch_request", _fetch)
write("ipc_decode", "05_published_frame", _published)
write("ipc_decode", "06_tick", _tick)
write("ipc_decode", "07_fetch_reply", _reply)
write("ipc_decode", "08_framed", _st.pack("<I", len(_published)) + _published)
write("ipc_decode", "10_compositing_frame", _published2)
write("ipc_decode", "09_huge_claims", b"\x01" + _st.pack("<QII", 1, 16384, 16384) + _st.pack("<I", 0xFFFFFFFF))
# Milestone 36 (ADR 010): the renderer's setup lists its grants; the host
# revokes one with a command.
# Milestone 37: then the fingerprint policy (resolution, timezone, fonts).
_setup = b"\x00" + b"\x03" + _st.pack("<I", 100) + b"\x01\x01" + _st.pack("<ff", 800.0, 600.0) + b"\x00" + _st.pack("<I", 0)
write("ipc_decode", "11_setup_with_grants", _setup)
write("ipc_decode", "12_revoke_network", b"\x01\x05\x00")
# Milestone 38b: a page hidden; a visibility flag that is not 0 or 1.
write("ipc_decode", "15_set_hidden", b"\x01\x06\x00")
write("ipc_decode", "16_visibility_not_a_bool", b"\x01\x06\x07")
write("ipc_decode", "13_revoke_unknown_kind", b"\x01\x05\xff")
# Gap pass (M36's gap): a re-grant of both kinds; one with a bit no kind
# has; a grant sent as a page command (only `ToRenderer::Grant` carries one).
write("ipc_decode", "17_grant_both", b"\x06\x03")
write("ipc_decode", "18_grant_unknown_bit", b"\x06\x80")
write("ipc_decode", "19_grant_as_command", b"\x01\x07\x01")
# Gap pass (M4's gap): a pointer release, which ends `:active`.
write("ipc_decode", "20_pointer_up", b"\x01\x02\x04" + struct.pack("<f", 3.0) + struct.pack("<f", 4.0))

# Milestone 37: a request the browser's filter blocked, answered as such.
write("ipc_decode", "14_blocked_reply", b"\x04" + _st.pack("<Q", 9) + b"\x01\x03" + _s("trackers (||t.example^)"))

# ---- filter_list (Milestone 37, ADR 019: EasyList syntax and hosts files)
write("filter_list", "01_typical", "[Adblock Plus 2.0]\n! comment\n||ads.example^\n||cdn.example^$third-party,script\n/banner/*/ad_\n|https://start.example/\nend.js|\n@@||ads.example/ok^\n||must.example^$important\n/promo/$domain=news.example|~sports.news.example\n")
write("filter_list", "02_hosts", "# hosts\n0.0.0.0 tracker.example\n127.0.0.1 more.example\n::1 six.example\n0.0.0.0 localhost\n")
write("filter_list", "03_skipped", "##.ad\nexample.com##.banner\n/ads?[0-9]+/\n||x.example^$csp=script-src 'none'\n||y.example^$redirect=noop.js\n")
write("filter_list", "04_hostile", "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b\n||" + "a." * 120 + "example^\n^^^^^^^^^\n|||\n@@\n$\n$$$$third-party\n" + "x" * 1100 + "\n")

# ---- dns_message (Milestone 37, ADR 012: DoH responses)
_q = b"\x00\x00\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01"
_resp = b"\x00\x00\x81\x80" + _q[4:7] + b"\x01" + _q[8:] + b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x01\x2c\x00\x04\x5d\xb8\xd8\x22"
write("dns_message", "01_answer", _resp)
write("dns_message", "02_query", _q)
write("dns_message", "03_pointer_loop", b"\x00\x00\x81\x80\x00\x00\x00\x01\x00\x00\x00\x00\xc0\x0c")
write("dns_message", "04_cname_chain", b"\x00\x00\x81\x80" + _q[4:7] + b"\x02" + _q[8:] + b"\xc0\x0c\x00\x05\x00\x01\x00\x00\x00\x3c\x00\x02\xc0\x0c" + b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x01\x02\x03\x04")

# ---- profile_files (Milestone 36: settings, permissions, local storage) ----
write("profile_files", "01_settings", "browser-engine settings 1\nscroll.invert = true\nscroll.speed = 2.5\nzoom.default = 1.25\nprivacy.mode = false\n")
write("profile_files", "02_settings_hostile", "browser-engine settings 1\nscroll.speed = NaN\nzoom.default = 1e39\nscroll.speed = -0\nzoom.default = inf\n= =\nscroll.invert = TRUE\n")
write("profile_files", "03_permissions", "browser-engine permissions 1\nhttps://a.example\tcamera\tallow\nhttp://b.example:8080\tclipboard\tdeny\n")
write("profile_files", "04_permissions_hostile", "browser-engine permissions 1\nnull\tcamera\tallow\nhttps://a.example/x\tcamera\tallow\nhttps://a.example\tfilesystem\tallow\nhttps://xn--80ak6aa92e.com\tgeolocation\tdeny\n\t\t\t\n")
write("profile_files", "05_local_storage", "browser-engine local-storage 1\nhttps://a.example\tkey\tvalue\nhttps://a.example\ttab\\there\tnew\\nline\\\\\n")
write("profile_files", "06_local_storage_hostile", "browser-engine local-storage 1\nhttps://a.example\tk\\\n\\x\tv\nfile:///etc\tk\tv\nhttps://a.example\tk\n" + "https://a.example\tbig\t" + "x" * 4000 + "\n")

print("seeds written under", CORPUS)
