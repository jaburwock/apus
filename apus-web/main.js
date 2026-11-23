import init, { PointerKind, start } from "./apus_web.js";

const canvas = document.querySelector("canvas");
const status = document.querySelector("#status");

function showError(error) {
  status.textContent = error instanceof Error ? error.message : String(error);
  status.hidden = false;
  console.error(error);
}

try {
  await init();
  const app = await start(canvas);
  let pending = false;
  let stopped = false;
  let repeatTimer;
  let pointerId = null;
  function schedule() {
    clearTimeout(repeatTimer);
    if (!pending && !stopped) {
      pending = true;
      requestAnimationFrame(draw);
    }
  }
  function draw() {
    pending = false;
    try {
      const again = app.frame(canvas.clientWidth, canvas.clientHeight, window.devicePixelRatio);
      status.hidden = true;
      canvas.dataset.ready = "true";
      if (again) {
        schedule();
      } else {
        const delay = app.mouse_repeat_delay_ms();
        if (delay !== undefined) repeatTimer = setTimeout(schedule, Math.ceil(delay));
      }
    } catch (error) {
      stopped = true;
      showError(error);
    }
  }
  if (typeof app.frame_text === "function") canvas.frameText = () => app.frame_text();

  canvas.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || stopped || !app.key(event)) return;
    event.preventDefault();
    if (app.should_quit()) {
      window.location.reload();
      return;
    }
    schedule();
  });

  function forwardPointer(event, kind) {
    const rect = canvas.getBoundingClientRect();
    const inGrid = app.pointer((event.clientX - rect.left) / rect.width,
      (event.clientY - rect.top) / rect.height, kind);
    if (inGrid) schedule();
    return inGrid;
  }
  function cancelPointer() {
    if (pointerId === null) return;
    const id = pointerId;
    pointerId = null;
    app.cancel_pointer();
    if (canvas.hasPointerCapture(id)) canvas.releasePointerCapture(id);
    schedule();
  }
  canvas.addEventListener("pointerdown", (event) => {
    if (stopped || pointerId !== null || !event.isPrimary || event.button !== 0
        || !["mouse", "touch"].includes(event.pointerType)) return;
    if (!forwardPointer(event, PointerKind.Press)) return;
    pointerId = event.pointerId;
    canvas.setPointerCapture(pointerId);
    canvas.focus({ preventScroll: true });
  });
  canvas.addEventListener("pointermove", (event) => {
    if (stopped) return;
    if (event.pointerId === pointerId) {
      if (!(event.buttons & 1) || !forwardPointer(event, PointerKind.Drag)) cancelPointer();
    } else if (pointerId === null && event.isPrimary && event.pointerType === "mouse" && !event.buttons) {
      forwardPointer(event, PointerKind.Move);
    }
  });
  canvas.addEventListener("pointerup", (event) => {
    if (stopped || event.pointerId !== pointerId) return;
    if (event.button === 0 && forwardPointer(event, PointerKind.Release)) {
      pointerId = null;
    } else {
      cancelPointer();
    }
  });
  for (const type of ["pointercancel", "lostpointercapture"]) {
    canvas.addEventListener(type, (event) => {
      if (event.pointerId === pointerId) cancelPointer();
    });
  }
  canvas.addEventListener("blur", cancelPointer);
  window.addEventListener("blur", cancelPointer);
  function resize() {
    cancelPointer();
    schedule();
  }
  new ResizeObserver(resize).observe(canvas);
  window.addEventListener("resize", resize);
  canvas.focus({ preventScroll: true });
  schedule();
} catch (error) {
  showError(error);
}
