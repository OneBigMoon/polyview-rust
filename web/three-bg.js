(() => {
  const canvas = document.getElementById("bg-canvas");
  if (!canvas) return;

  const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ?? false;
  const pointer = { x: 0, y: 0 };
  let renderer;
  let scene;
  let camera;
  let world;
  let points;
  let rings = [];
  let links;
  let frameId = 0;
  let visible = true;
  let playing = false;

  const darkPalette = [
    0x8d51f9, 0xa78bfa, 0x4cc9f0, 0x60a5fa, 0xf472b6, 0xfbbf24,
  ];
  const lightPalette = [
    0x6d28d9, 0x7c3aed, 0x2563eb, 0x0891b2, 0xdb2777, 0xd97706,
  ];

  function isDark() {
    return document.documentElement.dataset.theme === "dark";
  }

  function paint() {
    if (!points) return;
    const dark = isDark();
    const palette = dark ? darkPalette : lightPalette;
    const color = new window.THREE.Color();
    const colors = points.geometry.attributes.color;
    for (let i = 0; i < colors.count; i += 1) {
      color.setHex(palette[i % palette.length]);
      color.toArray(colors.array, i * 3);
    }
    colors.needsUpdate = true;
    points.material.opacity = dark ? 0.96 : 0.78;
    points.material.size = dark ? 0.11 : 0.12;
    points.material.blending = dark
      ? window.THREE.AdditiveBlending
      : window.THREE.NormalBlending;
    points.material.needsUpdate = true;
    rings.forEach((ring) => {
      ring.material.opacity = dark ? 0.3 : 0.2;
      ring.material.color.setHex(dark ? 0xa78bfa : 0x6d28d9);
    });
    if (links) links.material.opacity = dark ? 0.16 : 0.11;
  }

  function resize() {
    if (!renderer || !camera) return;
    const width = window.innerWidth;
    const height = window.innerHeight;
    camera.aspect = width / Math.max(height, 1);
    camera.updateProjectionMatrix();
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 1.5));
    renderer.setSize(width, height, false);
  }

  function build() {
    const THREE = window.THREE;
    renderer = new THREE.WebGLRenderer({
      canvas,
      alpha: true,
      antialias: false,
      powerPreference: "low-power",
    });
    renderer.setClearColor(0x000000, 0);
    renderer.outputColorSpace = THREE.SRGBColorSpace;

    scene = new THREE.Scene();
    camera = new THREE.PerspectiveCamera(
      54,
      window.innerWidth / Math.max(window.innerHeight, 1),
      0.1,
      120,
    );
    camera.position.set(0, 0, 18);

    world = new THREE.Group();
    scene.add(world);

    const count = window.innerWidth < 720 ? 700 : 1200;
    const positions = new Float32Array(count * 3);
    const colors = new Float32Array(count * 3);
    const color = new THREE.Color();
    for (let i = 0; i < count; i += 1) {
      const radius = 6 + Math.random() * 26;
      const theta = Math.random() * Math.PI * 2;
      const phi = Math.acos(2 * Math.random() - 1);
      positions[i * 3] = radius * Math.sin(phi) * Math.cos(theta);
      positions[i * 3 + 1] = radius * Math.sin(phi) * Math.sin(theta) * 0.62;
      positions[i * 3 + 2] = radius * Math.cos(phi) * 0.8;
      color.setHex(lightPalette[i % lightPalette.length]);
      color.toArray(colors, i * 3);
    }

    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    geometry.setAttribute("color", new THREE.BufferAttribute(colors, 3));
    points = new THREE.Points(
      geometry,
      new THREE.PointsMaterial({
        size: 0.12,
        sizeAttenuation: true,
        vertexColors: true,
        transparent: true,
        depthWrite: false,
      }),
    );
    world.add(points);

    const linkPositions = [];
    for (let i = 0; i < 240; i += 1) {
      const a = Math.floor(Math.random() * count);
      const b = Math.floor(Math.random() * count);
      if (a === b) continue;
      const ax = positions[a * 3];
      const ay = positions[a * 3 + 1];
      const az = positions[a * 3 + 2];
      const bx = positions[b * 3];
      const by = positions[b * 3 + 1];
      const bz = positions[b * 3 + 2];
      const distance = Math.hypot(ax - bx, ay - by, az - bz);
      if (distance > 9) continue;
      linkPositions.push(ax, ay, az, bx, by, bz);
    }
    const linkGeometry = new THREE.BufferGeometry();
    linkGeometry.setAttribute(
      "position",
      new THREE.Float32BufferAttribute(linkPositions, 3),
    );
    links = new THREE.LineSegments(
      linkGeometry,
      new THREE.LineBasicMaterial({
        color: 0x8d51f9,
        transparent: true,
        depthWrite: false,
      }),
    );
    world.add(links);

    [
      [7.5, 0.16, 0.9],
      [11, 1.2, 0.2],
      [15, 2.2, -0.6],
    ].forEach(([radius, tilt, turn]) => {
      const ring = new THREE.Mesh(
        new THREE.TorusGeometry(radius, 0.028, 4, 220),
        new THREE.MeshBasicMaterial({
          color: 0x7c3aed,
          transparent: true,
          depthWrite: false,
        }),
      );
      ring.rotation.x = tilt;
      ring.rotation.y = turn;
      world.add(ring);
      rings.push(ring);
    });

    paint();
    resize();
    window.addEventListener("resize", resize, { passive: true });
    window.addEventListener(
      "pointermove",
      (event) => {
        pointer.x = (event.clientX / window.innerWidth) * 2 - 1;
        pointer.y = (event.clientY / window.innerHeight) * 2 - 1;
      },
      { passive: true },
    );
    document.addEventListener("visibilitychange", () => {
      visible = !document.hidden;
      if (visible && !reducedMotion && !playing) {
        cancelAnimationFrame(frameId);
        frameId = requestAnimationFrame(loop);
      }
    });
    const syncPlayback = () => {
      const next = document.body.dataset.playing === "1";
      const changed = next !== playing;
      playing = next;
      document.body.classList.toggle("is-playing", playing);
      if (playing) {
        cancelAnimationFrame(frameId);
        frameId = 0;
      } else if (changed && visible && !reducedMotion) {
        cancelAnimationFrame(frameId);
        frameId = requestAnimationFrame(loop);
      }
    };
    new MutationObserver(syncPlayback).observe(document.body, {
      attributes: true,
      attributeFilter: ["data-playing"],
    });
    const appShell = document.getElementById("app-shell");
    const syncMode = () => {
      document.body.classList.toggle("bg-browse", !!appShell && !appShell.hidden);
    };
    if (appShell)
      new MutationObserver(syncMode).observe(appShell, {
        attributes: true,
        attributeFilter: ["hidden"],
      });
    syncMode();
    syncPlayback();
    new MutationObserver(paint).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    if (reducedMotion) {
      renderer.render(scene, camera);
      return;
    }
    frameId = requestAnimationFrame(loop);
  }

  function loop(now) {
    if (!renderer || !visible || playing) return;
    const time = now * 0.00012;
    world.rotation.y = time * 0.6 + pointer.x * 0.14;
    world.rotation.x = Math.sin(time * 0.8) * 0.07 + pointer.y * 0.1;
    camera.position.x += (pointer.x * 1.1 - camera.position.x) * 0.025;
    camera.position.y += (-pointer.y * 0.8 - camera.position.y) * 0.025;
    camera.lookAt(0, 0, 0);
    rings.forEach((ring, index) => {
      ring.rotation.z = time * (0.12 + index * 0.05) * (index % 2 ? -1 : 1);
    });
    renderer.render(scene, camera);
    frameId = requestAnimationFrame(loop);
  }

  import("./vendor/three.module.min.js")
    .then((module) => {
      window.THREE = module;
      try {
        build();
      } catch {
        canvas.hidden = true;
      }
    })
    .catch(() => {
      canvas.hidden = true;
    });
})();
