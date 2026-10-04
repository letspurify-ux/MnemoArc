import * as THREE from 'three';
import { RoundedBoxGeometry } from 'three/addons/geometries/RoundedBoxGeometry.js';

// Both sizes share the same character; the small brand mark uses a portrait crop.
export function mountRobot(host, { variant = 'portrait' } = {}) {
  const standing = variant === 'standing';
  const renderer = new THREE.WebGLRenderer({ alpha: true, antialias: true, powerPreference: 'low-power' });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.setClearColor(0, 0);
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.2;

  const scene = new THREE.Scene();
  const viewSize = standing ? 2.68 : 1.78;
  const camera = new THREE.OrthographicCamera(-viewSize / 2, viewSize / 2, viewSize / 2, -viewSize / 2, 0.1, 30);
  const lookAt = new THREE.Vector3(0, standing ? 1.64 : 2.0, 0);
  camera.position.set(0.35, standing ? 2.2 : 2.18, 6);
  camera.lookAt(lookAt);
  scene.add(new THREE.HemisphereLight(0xf6f4ff, 0x9d92b0, 2.5));
  const key = new THREE.DirectionalLight(0xfff3df, 3.7);
  key.position.set(-3, 7, 5);
  scene.add(key);
  const fill = new THREE.DirectionalLight(0xc7ccff, 2);
  fill.position.set(4, 3, -3);
  scene.add(fill);

  const materials = new Set();
  const geometries = new Map();
  function material(color, roughness = 0.5, metalness = 0) {
    const value = new THREE.MeshStandardMaterial({ color, roughness, metalness });
    materials.add(value);
    return value;
  }
  function luminous(color) {
    const value = new THREE.MeshBasicMaterial({ color });
    materials.add(value);
    return value;
  }
  const pearl = material(0xffffff, 0.3, 0.06);
  // Use a clearer blue accent so the mascot reads as blue instead of washed-out lavender.
  const blue = material(0x4d8fce, 0.34, 0.14);
  const dark = material(0x222e43, 0.27, 0.08);
  const joint = material(0x68758f, 0.5, 0.18);
  const eyesMaterial = luminous(0xb0f7ec);

  function geometry(id, create) {
    if (!geometries.has(id)) geometries.set(id, create());
    return geometries.get(id);
  }
  function mesh(parent, shape, surface, x, y, z) {
    const value = new THREE.Mesh(shape, surface);
    value.position.set(x, y, z);
    parent.add(value);
    return value;
  }
  function box(parent, surface, x, y, z, w, h, d, radius = 0.04) {
    return mesh(parent, geometry(`box:${w}:${h}:${d}:${radius}`, () => new RoundedBoxGeometry(w, h, d, 2, radius)), surface, x, y, z);
  }
  function sphere(parent, surface, x, y, z, sx, sy, sz) {
    const value = mesh(parent, geometry('sphere', () => new THREE.SphereGeometry(1, 24, 16)), surface, x, y, z);
    value.scale.set(sx, sy, sz);
    return value;
  }
  function cylinder(parent, surface, x, y, z, top, bottom, height) {
    return mesh(parent, geometry(`cylinder:${top}:${bottom}:${height}`, () => new THREE.CylinderGeometry(top, bottom, height, 32)), surface, x, y, z);
  }
  function rod(parent, surface, from, to, radius) {
    const a = new THREE.Vector3(...from);
    const b = new THREE.Vector3(...to);
    const middle = a.clone().add(b).multiplyScalar(0.5);
    const value = cylinder(parent, surface, ...middle.toArray(), radius, radius, a.distanceTo(b));
    value.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), b.sub(a).normalize());
    return value;
  }
  // An extruded rounded outline keeps the visor corners soft even at a shallow depth.
  function panel(parent, surface, x, y, z, w, h, radius, depth = 0.035) {
    const shape = geometry(`panel:${w}:${h}:${radius}:${depth}`, () => {
      const path = new THREE.Shape();
      const l = -w / 2, r = w / 2, b = -h / 2, t = h / 2;
      path.moveTo(l + radius, b);
      path.lineTo(r - radius, b);
      path.quadraticCurveTo(r, b, r, b + radius);
      path.lineTo(r, t - radius);
      path.quadraticCurveTo(r, t, r - radius, t);
      path.lineTo(l + radius, t);
      path.quadraticCurveTo(l, t, l, t - radius);
      path.lineTo(l, b + radius);
      path.quadraticCurveTo(l, b, l + radius, b);
      return new THREE.ExtrudeGeometry(path, { depth, bevelEnabled: false, steps: 1, curveSegments: 8 });
    });
    return mesh(parent, shape, surface, x, y, z);
  }
  function tube(parent, surface, points, radius) {
    const curve = new THREE.QuadraticBezierCurve3(...points.map(p => new THREE.Vector3(...p)));
    return mesh(parent, geometry(`tube:${points.join(':')}:${radius}`, () => new THREE.TubeGeometry(curve, 16, radius, 8, false)), surface, 0, 0, 0);
  }

  const robot = new THREE.Group();
  scene.add(robot);
  const body = new THREE.Group();
  body.position.y = 1.21;
  robot.add(body);
  // A little pear-shaped body and oversized head give the standing mascot a softer silhouette.
  sphere(body, pearl, 0, 0, 0, 0.4, 0.37, 0.3);
  panel(body, blue, 0, 0.025, 0.294, 0.19, 0.17, 0.025, 0.018);
  cylinder(robot, joint, 0, 1.6, 0, 0.12, 0.14, 0.15);

  const head = new THREE.Group();
  head.position.y = 2.0;
  head.scale.setScalar(1.08);
  robot.add(head);
  box(head, pearl, 0, 0, 0, 1.19, 0.92, 0.77, 0.25);
  panel(head, blue, 0, -0.025, 0.365, 1.045, 0.69, 0.245);
  panel(head, dark, 0, -0.025, 0.405, 0.96, 0.61, 0.22);
  const eyes = [-0.225, 0.225].map(x => sphere(head, eyesMaterial, x, 0.005, 0.449, 0.059, 0.099, 0.024));
  tube(head, eyesMaterial, [[-0.085, -0.135, 0.45], [0, -0.22, 0.452], [0.085, -0.135, 0.45]], 0.013);
  // Small satin ear caps and a single offset antenna give the silhouette its character.
  [-1, 1].forEach(side => {
    sphere(head, blue, side * 0.603, -0.015, -0.01, 0.095, 0.18, 0.18);
    sphere(head, pearl, side * 0.668, -0.015, 0, 0.04, 0.1, 0.105);
  });
  rod(head, joint, [0.28, 0.42, -0.04], [0.36, 0.65, -0.04], 0.022);
  const antenna = sphere(head, blue, 0.36, 0.65, -0.04, 0.08, 0.08, 0.08);
  sphere(head, pearl, 0.34, 0.676, 0.019, 0.024, 0.024, 0.01);

  const arms = [];
  if (standing) {
    [-1, 1].forEach(side => {
      // Short legs and outward-facing boots keep both feet firmly on the ground.
      box(robot, joint, side * 0.205, 0.81, 0, 0.18, 0.27, 0.2, 0.07);
      const foot = new THREE.Group();
      foot.position.set(side * 0.225, 0.64, 0.06);
      foot.rotation.y = side * 0.15;
      robot.add(foot);
      box(foot, pearl, 0, 0, 0.035, 0.34, 0.23, 0.43, 0.1);
      box(foot, blue, 0, -0.095, 0.035, 0.33, 0.055, 0.42, 0.025);
      sphere(robot, blue, side * 0.39, 1.39, 0, 0.1, 0.1, 0.11);
      const arm = new THREE.Group();
      arm.position.set(side * 0.4, 1.38, 0);
      robot.add(arm);
      rod(arm, pearl, [0, 0, 0], [side * 0.115, -0.24, 0.04], 0.09);
      sphere(arm, pearl, side * 0.13, -0.31, 0.065, 0.125, 0.145, 0.11);
      sphere(arm, pearl, side * 0.045, -0.28, 0.14, 0.055, 0.075, 0.05);
      arms.push(arm);
    });
    // A soft oval contact shadow adds depth without a shadow-map render pass.
    const shadowMaterial = new THREE.ShaderMaterial({
      transparent: true,
      depthWrite: false,
      vertexShader: 'varying vec2 vUv; void main() { vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }',
      fragmentShader: 'varying vec2 vUv; void main() { float falloff = max(0.0, 1.0 - length((vUv - 0.5) * 2.0)); gl_FragColor = vec4(0.35, 0.29, 0.44, falloff * falloff * 0.18); }',
    });
    materials.add(shadowMaterial);
    const shadow = mesh(scene, geometry('contact-shadow', () => new THREE.PlaneGeometry(1.6, 0.9)), shadowMaterial, 0, 0.515, 0.08);
    shadow.rotation.x = -Math.PI / 2;
  }

  // Keep the mascot animated even when the OS disables visual effects.
  let visible = true;
  let lost = false;
  let disposed = false;
  let revealed = false;
  let targetX = 0;
  let targetY = 0;
  let elapsed = 0;
  let last = null;
  function frame(now) {
    if (last !== null && now - last < 1000 / 30) return;
    const dt = last === null ? 0 : Math.min((now - last) / 1000, 0.1);
    last = now;
    elapsed += dt;
    const t = elapsed;
    const ease = 1 - Math.exp(-6 * dt);
    head.rotation.y += (targetX * 0.35 + Math.sin(t * 0.7) * 0.045 - head.rotation.y) * ease;
    head.rotation.x += (targetY * 0.2 + Math.sin(t * 1.6) * 0.018 - head.rotation.x) * ease;
    head.rotation.z = Math.sin(t * 1.1) * 0.025;
    const blink = Math.max(0, 1 - Math.abs((t % 5.1) - 4.7) / 0.12);
    eyes.forEach(eye => { eye.scale.y = 0.099 * (1 - blink * 0.92); });
    body.scale.y = 1 + Math.sin(t * 1.8) * 0.015;
    arms.forEach((arm, index) => {
      const side = index === 0 ? -1 : 1;
      arm.rotation.z = side * (0.04 + Math.sin(t * 1.8) * 0.035);
    });
    antenna.position.y = 0.65 + Math.sin(t * 1.6) * 0.008;
    renderer.render(scene, camera);
    // Start the fade only once a complete frame is available, including after context recovery.
    if (!revealed) {
      revealed = true;
      host.classList.add('robot-ready');
    }
  }
  function sync() {
    renderer.setAnimationLoop(null);
    last = null;
    if (disposed || lost || !visible || document.hidden) return;
    frame(performance.now());
    renderer.setAnimationLoop(frame);
  }
  function resize() {
    const { width, height } = host.getBoundingClientRect();
    renderer.setSize(Math.max(width, 1), Math.max(height, 1), false);
    const aspect = width / (height || 1);
    camera.left = -viewSize * aspect / 2;
    camera.right = viewSize * aspect / 2;
    camera.updateProjectionMatrix();
    sync();
  }
  function pointer(event) {
    const rect = host.getBoundingClientRect();
    // Track the whole page relative to each robot, with a gradual response outside its icon.
    targetX = THREE.MathUtils.clamp((event.clientX - rect.left - rect.width / 2) / 250, -1, 1);
    targetY = THREE.MathUtils.clamp((event.clientY - rect.top - rect.height / 2) / 250, -1, 1);
  }
  function reset() { targetX = 0; targetY = 0; }
  function contextLost(event) { event.preventDefault(); lost = true; revealed = false; host.classList.remove('robot-ready'); sync(); }
  function contextRestored() { lost = false; sync(); }
  const observer = new ResizeObserver(resize);
  const intersection = new IntersectionObserver(entries => { visible = entries[0].isIntersecting; sync(); });
  renderer.domElement.setAttribute('aria-hidden', 'true');
  host.appendChild(renderer.domElement);
  resize();
  observer.observe(host);
  intersection.observe(host);
  window.addEventListener('pointermove', pointer, { passive: true });
  window.addEventListener('blur', reset);
  document.addEventListener('visibilitychange', sync);
  renderer.domElement.addEventListener('webglcontextlost', contextLost);
  renderer.domElement.addEventListener('webglcontextrestored', contextRestored);
  return () => {
    disposed = true;
    renderer.setAnimationLoop(null);
    observer.disconnect();
    intersection.disconnect();
    window.removeEventListener('pointermove', pointer);
    window.removeEventListener('blur', reset);
    document.removeEventListener('visibilitychange', sync);
    renderer.domElement.removeEventListener('webglcontextlost', contextLost);
    renderer.domElement.removeEventListener('webglcontextrestored', contextRestored);
    geometries.forEach(value => value.dispose());
    materials.forEach(value => value.dispose());
    renderer.dispose();
    renderer.forceContextLoss();
    renderer.domElement.remove();
    host.classList.remove('robot-ready');
  };
}
