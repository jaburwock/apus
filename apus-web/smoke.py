"""Exercise the built demo with keyboard, mouse and touch in Chromium."""

import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import re
from tempfile import TemporaryDirectory
from threading import Thread

from playwright.sync_api import sync_playwright

DELETION = 108_190_703
INSERTION = 152_304_195
CENTRE_PATTERN = r"│g\.([\d,]+)\s*$"


def rendered_rows(canvas):
    return canvas.evaluate("(canvas) => canvas.frameText()").splitlines()


def frame_at(page, canvas, position):
    suffix = f"g.{position:,}"
    page.wait_for_function(
        """suffix => document.querySelector("canvas").frameText().split("\\n")
            .some(row => row.trimEnd().endsWith(suffix))""",
        arg=suffix,
    )
    rows = rendered_rows(canvas)
    centre_row = next(i for i, row in enumerate(rows) if row.rstrip().endswith(suffix))
    centre_column = rows[centre_row].index(suffix) - 1
    return rows, rows[centre_row - 1], centre_column


def command(page, text):
    page.keyboard.type(f":{text}")
    page.keyboard.press("Enter")


def load_demo(page, url):
    page.goto(url)
    canvas = page.locator("canvas")
    page.wait_for_function(
        """() => document.querySelector("canvas").dataset.ready
            || !document.querySelector("#status").hidden
                && document.querySelector("#status").textContent !== "Loading the HG002 demo…" """
    )
    assert canvas.get_attribute("data-ready") == "true", page.locator("#status").text_content()
    assert canvas.evaluate("(canvas) => canvas.width === canvas.clientWidth * devicePixelRatio")
    assert canvas.evaluate("(canvas) => typeof canvas.frameText === 'function'"), "Build with APUS_WEB_SMOKE=1"
    assert "HG002 (2 variants)" in "\n".join(rendered_rows(canvas))
    assert page.get_by_role("link", name="Data credits").is_visible()
    return canvas


def check_demo(page, url, screenshots):
    requests = []
    errors = []
    page.on("request", lambda request: requests.append(request.url))
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("console", lambda msg: errors.append(msg.text) if msg.type == "error" else None)

    canvas = load_demo(page, url)
    images = [canvas.screenshot()]

    # The only requests should be the static app, even while jumping between both windows.
    page.context.set_offline(True)
    for row, (position, low, high, prefix, suffix) in enumerate([
        (DELETION, 108_140_703, 108_243_180, "TAAAATAAGT", "CCTATTGGGC"),
        (INSERTION, 152_254_195, 152_354_195, "TTTAATCCCC", "ACCTGAGTCA"),
    ]):
        if row == 1:
            page.keyboard.press("Tab")
            page.wait_for_function("document.querySelector('canvas').frameText().includes('Current line:')")
            page.keyboard.press("ArrowDown")
        page.keyboard.press("Enter")
        rows, _, _ = frame_at(page, canvas, position)
        assert any(re.search(r"Depth: [1-9][0-9]*X", line) for line in rows), rows
        assert any("Average depth over span" in line for line in rows), rows
        chart = row_below(rows, "Region span:")
        assert re.search(r"[1-9]", rows[chart][:5]), rows[chart]
        images.append(canvas.screenshot())

        command(page, "set movemin 1")
        page.keyboard.press("ArrowRight")
        frame_at(page, canvas, position + 1)
        page.keyboard.press("ArrowLeft")
        rows, _, _ = frame_at(page, canvas, position)
        span = int(re.search(r"Pileup span: ([\d,]+)bp", "\n".join(rows))[1].replace(",", ""))
        for zoom_step in range(3):
            if zoom_step:
                page.keyboard.press("-")
            for target, edge, key, expected, before, after in [
                (low + 1, low, "ArrowLeft", "----" + prefix, 4, 10),
                (high - 1, high, "ArrowRight", suffix + "----", 9, 5),
            ]:
                command(page, f"goto {target}")
                frame_at(page, canvas, target)
                page.keyboard.press(key)
                page.keyboard.press(key)
                rows, reference, centre = frame_at(page, canvas, edge)
                if zoom_step < 2:
                    assert reference[centre - before:centre + after] == expected, reference
                else:
                    assert any(f"Pileup span: {span * 2:,}bp" in line for line in rows), rows
                assert "⣿" in rows[row_below(rows, "Region span:")], rows
        page.keyboard.press("+")
        page.keyboard.press("+")

    assert len(set(images)) == 3, "Expected distinct canvas images for the table and both pileups"
    assert set(requests) == {url + path for path in ("", "main.js", "apus_web.js", "apus_web_bg.wasm")}, requests
    assert not errors, errors
    if screenshots:
        screenshots.mkdir(parents=True, exist_ok=True)
        for name, image in zip(("webgl2-vcf.png", "webgl2-deletion.png", "webgl2-insertion.png"), images):
            (screenshots / name).write_bytes(image)


def row_below(rows, label):
    return next(i + 1 for i, row in enumerate(rows) if label in row)


def cell_point(canvas, column, row):
    bounds = canvas.bounding_box()
    rows = rendered_rows(canvas)
    # The unused backing-store strip is smaller than one cell on either axis.
    return (
        bounds["x"] + bounds["width"] * (column + 0.5) / (len(rows[0]) + 0.5),
        bounds["y"] + bounds["height"] * (row + 0.5) / (len(rows) + 0.5),
    )


def wait_for_row(page, text, present=True):
    page.wait_for_function(
        r"""({text, present}) => document.querySelector("canvas").frameText().split("\n")
            .some(row => row.includes(text)) === present""",
        arg={"text": text, "present": present},
    )


def cell_with_text(canvas, text, toolbar=False):
    rows = rendered_rows(canvas)
    row = 0 if toolbar else next(i for i, row in enumerate(rows) if text in row)
    return cell_point(canvas, rows[row].index(text) + len(text) // 2, row)


def touch_sender(page):
    session = page.context.new_cdp_session(page)

    def send_touch(kind, point=None):
        points = [] if point is None else [{"x": point[0], "y": point[1]}]
        session.send("Input.dispatchTouchEvent", {"type": kind, "touchPoints": points})

    return send_touch


def check_controls(page, url, touch):
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    canvas = load_demo(page, url)
    click = page.touchscreen.tap if touch else page.mouse.click
    send_touch = touch_sender(page) if touch else None

    def drag(*points):
        start, *moves = points
        if touch:
            send_touch("touchStart", start)
            for point in moves:
                send_touch("touchMove", point)
            send_touch("touchEnd")
        else:
            page.mouse.move(*start)
            page.mouse.down()
            for point in moves:
                page.mouse.move(*point)
            page.mouse.up()

    table_start = row_below(rendered_rows(canvas), "CHROM")
    first_record = cell_point(canvas, 2, table_start)
    for button, heading in [("Settings", "Settings "), ("Hotkeys", "Shortcuts ")]:
        click(*cell_with_text(canvas, button, toolbar=True))
        wait_for_row(page, "┌")
        assert heading in "\n".join(rendered_rows(canvas)[1:])
        click(*first_record)
        wait_for_row(page, "┌", present=False)
        assert "Current line:" in "\n".join(rendered_rows(canvas))

    if not touch:
        command(page, "columnpicker")
        wait_for_row(page, "Columns [")
        rows = rendered_rows(canvas)
        border_row = next(i for i, row in enumerate(rows) if "Columns [" in row)
        click(*cell_point(canvas, rows[border_row].index("┌"), border_row))
        assert "Columns [" in "\n".join(rendered_rows(canvas))
        click(*first_record)
        wait_for_row(page, "Columns [", present=False)

    drag(first_record, cell_point(canvas, 6, table_start), first_record)
    assert "Current line:" in "\n".join(rendered_rows(canvas)), "A drag back to the press cell must never jump"
    drag(cell_point(canvas, 14, table_start + 1), cell_point(canvas, 2, table_start))
    wait_for_row(page, "Current line: 2 ")
    wait_for_row(page, "CHROM", present=False)
    click(*first_record)
    frame_at(page, canvas, INSERTION)

    for current, next_label in [("Light", "Dark"), ("Dark", "RdGrn"), ("RdGrn", "Light")]:
        click(*cell_with_text(canvas, current, toolbar=True))
        wait_for_row(page, next_label)
    if not touch:
        command(page, "palette")
        wait_for_row(page, "Select palette")
        page.keyboard.press("ArrowDown")
        page.keyboard.press("Enter")
        wait_for_row(page, "default dark")
        page.keyboard.press("Enter")
        wait_for_row(page, "Dark")
        page.keyboard.press(",")
        wait_for_row(page, "default dark", present=False)
    command(page, "set colorscheme red-green dark")
    wait_for_row(page, "RdGrn")
    command(page, "set palette surface_app_bg:purple(3)")
    wait_for_row(page, "Custom")
    click(*cell_with_text(canvas, "Custom", toolbar=True))
    wait_for_row(page, "Light")

    click(*cell_with_text(canvas, "Tracks", toolbar=True))
    wait_for_row(page, "[x] Local area")
    click(*cell_with_text(canvas, "[x] Local area"))
    wait_for_row(page, "[ ] Local area")
    wait_for_row(page, "Region span:", present=False)
    click(*cell_with_text(canvas, "[ ] Local area"))
    wait_for_row(page, "Region span:")
    click(*cell_with_text(canvas, "Settings", toolbar=True))
    wait_for_row(page, "[x] Local area", present=False)
    assert not any("┌" in row and "Settings " in row for row in rendered_rows(canvas)[1:]), (
        "A dismissal press must not open Settings"
    )

    command(page, "set readcolor lane")
    for current, next_mode in [("Lane", "Direction"), ("Direction", "5mC"), ("5mC", "Lane")]:
        click(*cell_with_text(canvas, "Reads: " + current))
        wait_for_row(page, "Reads: " + next_mode)
        page.keyboard.press("-")
    page.keyboard.press("r")
    wait_for_row(page, "Reads: Direction")
    assert not errors, errors


def centre_position(canvas):
    for row in rendered_rows(canvas):
        centre = re.search(CENTRE_PATTERN, row)
        if centre:
            return int(centre[1].replace(",", ""))
    raise AssertionError("The pileup centre is missing")


def wait_for_pan(page, position):
    return page.wait_for_function(
        r"""({pattern, position}) => {
            const centre = document.querySelector("canvas").frameText().split("\n")
                .map(row => row.match(new RegExp(pattern))).find(Boolean);
            const next = Number(centre?.[1].replaceAll(",", ""));
            return next && next !== position ? next : false;
        }""",
        arg={"pattern": CENTRE_PATTERN, "position": position},
    ).json_value()


def check_pointer(page, url, touch):
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    canvas = load_demo(page, url)
    page.context.set_offline(True)
    click = page.touchscreen.tap if touch else page.mouse.click
    rows = rendered_rows(canvas)
    table_start = row_below(rows, "CHROM")
    click(*cell_point(canvas, 2, table_start - 1))
    click(*cell_point(canvas, 2, table_start + 2))
    if not touch:
        page.mouse.move(*cell_point(canvas, 2, table_start))
        page.mouse.down()
        page.mouse.move(*cell_point(canvas, 5, table_start), steps=3)
        page.mouse.up()
    assert "Current line:" in "\n".join(rendered_rows(canvas))

    for row, position in enumerate((DELETION, INSERTION)):
        if row:
            vcf_column = rendered_rows(canvas)[0].index("VCF") + 1
            click(*cell_point(canvas, vcf_column, 0))
            page.wait_for_function("document.querySelector('canvas').frameText().includes('Current line:')")
        click(*cell_point(canvas, 2, table_start + row))
        rows, _, _ = frame_at(page, canvas, position)
        assert "Region span:" in "\n".join(rows)

    pileup = row_below(rows, "Pileup span:")
    start = cell_point(canvas, 12, pileup)
    end = cell_point(canvas, 7, pileup)
    if touch:
        send_touch = touch_sender(page)
        send_touch("touchStart", start)
        send_touch("touchMove", end)
        send_touch("touchEnd")
    else:
        page.mouse.move(*start)
        page.mouse.down()
        page.mouse.move(*end)
        page.mouse.up()
    panned = wait_for_pan(page, INSERTION)
    assert panned == INSERTION + 5, panned

    # Wait for the frame loop to settle before starting the hold.
    page.wait_for_timeout(160)
    chart = row_below(rendered_rows(canvas), "Region span:")
    target = cell_point(canvas, len(rows[0]) // 2 + 3, chart)
    if touch:
        send_touch("touchStart", target)
    else:
        page.mouse.move(*target)
        page.mouse.down()
    first = wait_for_pan(page, panned)
    assert wait_for_pan(page, first) > first, "A held press should repeat from idle"
    if touch:
        send_touch("touchCancel")
    else:
        page.mouse.up()
    stopped = centre_position(canvas)
    page.wait_for_timeout(450)
    assert centre_position(canvas) == stopped, "Release or cancellation should stop the hold"

    if not touch:
        for cancel in ("leave", "resize"):
            page.mouse.move(*target)
            page.mouse.down()
            wait_for_pan(page, stopped)
            if cancel == "leave":
                page.mouse.move(5, page.viewport_size["height"] - 2)
            else:
                page.set_viewport_size({"width": 1240, "height": 690})
            page.wait_for_timeout(80)
            stopped = centre_position(canvas)
            page.wait_for_timeout(450)
            assert centre_position(canvas) == stopped, f"{cancel} should stop the hold"
            page.mouse.up()

    assert not errors, errors
    page.context.set_offline(False)
    page.get_by_role("link", name="Data credits").click()
    assert page.url == url + "data-credits.txt"
    assert "all 530 alignments" in page.locator("body").inner_text()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=Path(__file__).with_name("dist"))
    parser.add_argument("--screenshots", type=Path)
    args = parser.parse_args()
    with TemporaryDirectory() as root, sync_playwright() as playwright:
        (Path(root) / "apus").symlink_to(args.dist.resolve(), target_is_directory=True)
        handler = partial(SimpleHTTPRequestHandler, directory=root)
        with ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
            run_checks(server, playwright, args.screenshots)


def run_checks(server, playwright, screenshots):
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        browser = playwright.chromium.launch(
            channel="chromium",
            args=["--use-gl=angle", "--use-angle=swiftshader-webgl", "--enable-unsafe-swiftshader"],
        )
        url = f"http://127.0.0.1:{server.server_port}/apus/"
        for width, height, dpr, touch in [
            (1280, 720, 2, False), (1280, 720, 1, False), (390, 844, 2, True), (844, 390, 2, True),
        ]:
            page = browser.new_page(
                viewport={"width": width, "height": height}, device_scale_factor=dpr,
                has_touch=touch, is_mobile=touch,
            )
            page.set_default_timeout(8000)
            if not touch and dpr == 2:
                check_demo(page, url, screenshots)
            else:
                check_pointer(page, url, touch)
                if not touch or width < height:
                    check_controls(page, url, touch)
            page.context.close()
        print(f"Chromium {browser.version} WebGL2 smoke test passed")
        browser.close()
    finally:
        server.shutdown()
        thread.join()


if __name__ == "__main__":
    main()
