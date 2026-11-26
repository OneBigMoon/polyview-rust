(() => {
  const canvas = document.getElementById("bg-canvas");
  if (!canvas) return;
  let gl;
  try {
    gl = canvas.getContext("webgl2", { alpha: true, antialias: false, powerPreference: "low-power" });
  } catch { /* WebGL can be disabled by the browser. */ }
  if (!gl) { canvas.hidden = true; return; }

  import("./vendor/three.module.min.js").then(build).catch(() => { canvas.hidden = true; });

  function build(THREE) {
    const renderer = new THREE.WebGLRenderer({ canvas, context: gl, alpha: true });
    renderer.setClearColor(0x000000, 0);
    renderer.outputColorSpace = THREE.SRGBColorSpace;
    const scene = new THREE.Scene();
    const camera = new THREE.PerspectiveCamera(50, 1, 0.1, 85);
    const world = new THREE.Group();
    scene.add(world);

    const bitmap = document.createElement("canvas");
    bitmap.width = bitmap.height = 64;
    const ctx = bitmap.getContext("2d");
    if (!ctx) { renderer.dispose(); canvas.hidden = true; return; }
    const glow = ctx.createRadialGradient(32, 32, 0, 32, 32, 32);
    for (const [radius, alpha] of [[0, 1], [.12, .91], [.28, .58], [.5, .2], [.76, .035], [1, 0]])
      glow.addColorStop(radius, `rgba(255,255,255,${alpha})`);
    ctx.fillStyle = glow;
    ctx.fillRect(0, 0, 64, 64);
    const texture = new THREE.CanvasTexture(bitmap);
    texture.colorSpace = THREE.SRGBColorSpace;

    function dust(count, depth, spread, size, colorOffsets) {
      const positions = new Float32Array(count * 3);
      const colors = new Float32Array(count * 3);
      const tint = new THREE.Color();
      for (let i = 0; i < count; i++) {
        const angle = Math.random() * 2 * Math.PI;
        const radius = Math.sqrt(Math.random());
        positions[i * 3] = Math.cos(angle) * radius * spread;
        positions[i * 3 + 1] = Math.sin(angle) * radius * spread * .56;
        positions[i * 3 + 2] = depth + (Math.random() - .5) * 5;
        tint.setHex(colorOffsets[i % colorOffsets.length]).toArray(colors, i * 3);
      }
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
      geometry.setAttribute("color", new THREE.BufferAttribute(colors, 3));
      const field = new THREE.Points(geometry, new THREE.PointsMaterial({ map: texture, size,
        sizeAttenuation: true, transparent: true, vertexColors: true, depthWrite: false }));
      world.add(field);
      return field;
    }
    const compact = window.innerWidth < 720;
    const far = dust(compact ? 540 : 1000, -9, 32, .22, [0xffffff, 0xc7beff, 0xc9eaff]);
    const mist = dust(compact ? 14 : 24, -13, 34, 5.4, [0xffffff, 0xe2c4ff, 0xbadfff]);
    const near = dust(compact ? 12 : 22, 4, 19, 1.9, [0xffffff, 0xffe1c9, 0xedd7ff]);

    const tracks = new THREE.Group();
    world.add(tracks);
    const arcs = [7.5, 11, 15].map((radius, i) => {
      const group = new THREE.Group();
      const path = new THREE.TorusGeometry(radius, .018, 5, 168);
      const halo = new THREE.TorusGeometry(radius, .07, 5, 168);
      for (const geometry of [path, halo]) group.add(new THREE.Mesh(geometry, new THREE.MeshBasicMaterial({
        transparent: true, depthWrite: false,
      })));
      group.scale.set(1.24, 1, 1);
      group.rotation.set(.78 + i * .28, -.3 + i * .46, -.4 + i * .25);
      tracks.add(group);
      return group;
    });
    const vertices = far.geometry.attributes.position;
    const lines = [];
    for (let i = 0; i < Math.min(vertices.count, 180); i++) {
      let closest = -1, best = 39;
      for (let j = i + 1; j < Math.min(vertices.count, 180); j++) {
        const distance = (vertices.getX(i) - vertices.getX(j)) ** 2 + (vertices.getY(i) - vertices.getY(j)) ** 2 + (vertices.getZ(i) - vertices.getZ(j)) ** 2;
        if (distance < best) { best = distance; closest = j; }
      }
      if (closest >= 0) for (const k of [i, closest]) lines.push(vertices.getX(k), vertices.getY(k), vertices.getZ(k));
    }
    const lineGeometry = new THREE.BufferGeometry();
    lineGeometry.setAttribute("position", new THREE.Float32BufferAttribute(lines, 3));
    const links = new THREE.LineSegments(lineGeometry, new THREE.LineBasicMaterial({ transparent: true, depthWrite: false }));
    world.add(links);

    // Far dust, orbital light and foreground glow; pitch, camera distance and intensity.
    const looks = {
      auth: { dark: [0xab90ff, 0x55d7ff, 0xffb567], light: [0xb49aef, 0x80bcec, 0xf1aeca], pitch: -.1, z: 17, gain: 1 },
      online: { dark: [0xb789ff, 0x9786ff, 0xffbc67], light: [0xa88be7, 0x9fb7ee, 0xf3b49e], pitch: .08, z: 18, gain: .93 },
      live: { dark: [0x55cfff, 0x30e7eb, 0x859bff], light: [0x84c5e8, 0x60bfce, 0xb0b5f0], pitch: -.22, z: 16.8, gain: 1 },
      cloud: { dark: [0x80bcff, 0x4ce2d3, 0xcf9aff], light: [0x86ace0, 0x70bdb7, 0xc8a6dd], pitch: .27, z: 17.5, gain: .88 },
      library: { dark: [0x918cde, 0x739ce8, 0xbc9aca], light: [0xada6ce, 0xaabbd8, 0xccb8d7], pitch: -.04, z: 19, gain: .68 },
      settings: { dark: [0x8893bd, 0x879abc, 0x9da3be], light: [0xb7bdca, 0xa6bdcc, 0xc5c0cf], pitch: .02, z: 20, gain: .42 },
    };
    const targets = [new THREE.Color(), new THREE.Color(), new THREE.Color()];
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    const root = document.documentElement;
    let look, view, dark, soft, fixed, contextLost = false, initial = true;
    let pointerX = 0, pointerY = 0, focusX = 0, focusY = 0;
    let pulse = 0, time = 0, ringTime = 0, lastTime = 0, frame = 0;
    let width = 0, height = 0, ratio = 0;
    const materials = [far.material, mist.material, near.material, links.material, ...arcs.flatMap(arc => arc.children.map(mesh => mesh.material))];

    function render(now) {
      frame = 0;
      if (canvas.hidden) return;
      const dt = lastTime ? Math.min((now - lastTime) / 1000, .1) : 0;
      lastTime = now;
      const blend = initial || fixed ? 1 : 1 - Math.exp(-dt * 3.4);
      initial = false;
      const speed = fixed ? 0 : soft ? .25 : 1;
      time += dt * speed;
      pulse = Math.max(0, pulse - dt * 1.1);
      ringTime += dt * speed * (1 + pulse * 3);
      const scroll = fixed ? 0 : Math.min((window.scrollY || 0) / Math.max(height, 1), 3);
      const tv = root.dataset.tv === "1";
      const x = (tv ? focusX : pointerX) * speed, y = (tv ? focusY : pointerY) * speed;
      world.rotation.x += (look.pitch + .055 * y + scroll * .022 - world.rotation.x) * blend;
      world.rotation.y += (.11 * x - world.rotation.y) * blend;
      far.rotation.z = time * .003;
      mist.rotation.z = -.13 - time * .002;
      mist.scale.setScalar(1 + .027 * Math.sin(time * .17));
      near.position.x += (.65 * x - near.position.x) * blend;
      near.position.y += (-.5 * y + .2 * scroll - near.position.y) * blend;
      tracks.rotation.y = .15 * Math.sin(time * .07);
      arcs.forEach((arc, i) => {
        arc.rotation.z = -.4 + i * .25 + ringTime * (.008 + i * .003) * (i % 2 ? -1 : 1);
        for (const mesh of arc.children) mesh.material.color.lerp(targets[1], blend);
        arc.children[0].material.opacity = look.gain * (.24 + (view === "live" ? .09 * Math.sin(time * .9 + i) : .025 * pulse));
        arc.children[1].material.opacity = look.gain * .045;
      });
      far.material.color.lerp(targets[0], blend);
      mist.material.color.lerp(targets[0], blend);
      near.material.color.lerp(targets[2], blend);
      links.material.color.lerp(targets[1], blend);
      far.material.opacity = look.gain * (dark ? .91 : .8);
      mist.material.opacity = look.gain * .23;
      near.material.opacity = look.gain * .24;
      links.material.opacity += ((view === "cloud" ? .37 : .045) * look.gain - links.material.opacity) * blend;
      camera.position.x += (x * 1.1 - camera.position.x) * blend;
      camera.position.y += (-y * .8 - camera.position.y) * blend;
      camera.position.z += (look.z - scroll * .55 - camera.position.z) * blend;
      camera.lookAt(0, 0, 0);
      renderer.render(scene, camera);
      if (!fixed) frame = requestAnimationFrame(render);
    }
    function sync() {
      cancelAnimationFrame(frame);
      frame = 0;
      lastTime = 0;
      view = document.body.dataset.view || "auth";
      document.body.classList.toggle("bg-browse", view !== "auth");
      canvas.hidden = contextLost || document.hidden || document.body.dataset.playing === "1";
      if (canvas.hidden) return;
      look = looks[view] || (view === "downloads" ? looks.settings : looks.library);
      dark = root.dataset.theme === "dark";
      soft = root.dataset.bgMode === "soft";
      fixed = reduced?.matches || root.dataset.bgMode === "static";
      const palette = dark ? look.dark : look.light;
      targets.forEach((color, i) => color.setHex(palette[i]));
      const blend = dark ? THREE.AdditiveBlending : THREE.NormalBlending;
      for (const material of materials) if (material.blending !== blend) { material.blending = blend; material.needsUpdate = true; }
      const dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      if (width !== window.innerWidth || height !== window.innerHeight || ratio !== dpr) {
        width = Math.max(1, window.innerWidth); height = Math.max(1, window.innerHeight); ratio = dpr;
        camera.aspect = width / height;
        camera.updateProjectionMatrix();
        renderer.setPixelRatio(ratio);
        renderer.setSize(width, height, false);
      }
      frame = requestAnimationFrame(render);
    }
    window.addEventListener("pointermove", (event) => {
      if (!fixed && !canvas.hidden) { pointerX = event.clientX / width * 2 - 1; pointerY = event.clientY / height * 2 - 1; }
    }, { passive: true });
    document.addEventListener("focusin", (event) => {
      if (root.dataset.tv !== "1" || fixed || canvas.hidden) return;
      const bounds = event.target.getBoundingClientRect?.();
      if (bounds) { focusX = (bounds.left + bounds.width / 2) / width * 2 - 1; focusY = (bounds.top + bounds.height / 2) / height * 2 - 1; }
    });
    window.addEventListener("resize", sync, { passive: true });
    document.addEventListener("visibilitychange", sync);
    reduced?.addEventListener?.("change", sync);
    const observer = new MutationObserver((changes) => {
      if (changes.some(change => change.attributeName === "data-spotlight") && view === "online" && !fixed) pulse = 1;
      sync();
    });
    observer.observe(root, { attributes: true, attributeFilter: ["data-theme", "data-tv", "data-bg-mode"] });
    observer.observe(document.body, { attributes: true, attributeFilter: ["data-view", "data-playing", "data-spotlight"] });
    canvas.addEventListener("webglcontextlost", event => { event.preventDefault(); contextLost = true; sync(); });
    canvas.addEventListener("webglcontextrestored", () => { contextLost = false; initial = true; sync(); });
    sync();
  }
})();
