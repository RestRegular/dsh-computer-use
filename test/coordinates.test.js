import test from 'node:test';
import assert from 'node:assert/strict';

import { remember, resolvePoint, resolveTo } from '../lib/coordinates.js';

test('image pixels map into the captured rectangle', () => {
  remember('a', {
    capture: { x: 0, y: 0, width: 1920, height: 1200 },
    image: { width: 1600, height: 1000 },
    target: 'screen',
  });
  assert.deepEqual(resolvePoint('a', { x: 800, y: 500 }), { x: 960, y: 600, space: 'image' });
  assert.deepEqual(resolvePoint('a', { x: 0, y: 0 }), { x: 0, y: 0, space: 'image' });
  assert.deepEqual(resolvePoint('a', { x: 1600, y: 1000 }), { x: 1920, y: 1200, space: 'image' });
});

test('negative virtual desktop origins are preserved', () => {
  remember('b', {
    capture: { x: -1920, y: 0, width: 1920, height: 1080 },
    image: { width: 960, height: 540 },
    target: 'monitor',
  });
  assert.deepEqual(resolvePoint('b', { x: 0, y: 0 }), { x: -1920, y: 0, space: 'image' });
  assert.deepEqual(resolvePoint('b', { x: 960, y: 540 }), { x: 0, y: 1080, space: 'image' });
});

test('screen space bypasses the screenshot mapping', () => {
  assert.deepEqual(resolvePoint('missing', { x: 12.6, y: -3.2, coordinate_space: 'screen' }), {
    x: 13,
    y: -3,
    space: 'screen',
  });
});

test('missing screenshot rejects image-space coordinates', () => {
  assert.throws(() => resolvePoint('none', { x: 1, y: 2 }), /SCREENSHOT_REQUIRED/);
});

test('drag end coordinates use the same mapping', () => {
  remember('c', {
    capture: { x: 100, y: 200, width: 800, height: 600 },
    image: { width: 400, height: 300 },
    target: 'window',
  });
  assert.deepEqual(resolveTo('c', { to_x: 400, to_y: 300 }), { x: 900, y: 800, space: 'image' });
});
