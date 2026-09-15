// The two pictures of the model: how training is going, and what the network
// currently believes about the plane it is classifying.

import * as model from '../sim/model.js';
import { RADIUS } from '../sim/dataset.js';

const LOSS_COLOR = '#38bdf8';
const ACCURACY_COLOR = '#2dd4bf';
const CHECKPOINT_COLOR = '#a78bfa';
const GRID_COLOR = 'rgba(148, 163, 184, 0.14)';
const TEXT_COLOR = '#64748b';

function prepare(canvas) {
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (canvas.width !== Math.round(width * ratio) || canvas.height !== Math.round(height * ratio)) {
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
  }
  const context = canvas.getContext('2d');
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  return { context, width, height };
}

/** Loss on the left axis, held-out accuracy on the right, epochs across. */
export class TrainingChart {
  constructor(canvas) {
    this.canvas = canvas;
  }

  draw(snapshot) {
    const { context, width, height } = prepare(this.canvas);
    const padding = { top: 14, right: 40, bottom: 22, left: 40 };
    const plotWidth = width - padding.left - padding.right;
    const plotHeight = height - padding.top - padding.bottom;
    const history = snapshot.history;

    context.font = '10px ui-monospace, monospace';
    context.fillStyle = TEXT_COLOR;
    context.strokeStyle = GRID_COLOR;
    context.lineWidth = 1;

    for (let step = 0; step <= 4; step += 1) {
      const y = padding.top + (plotHeight * step) / 4;
      context.beginPath();
      context.moveTo(padding.left, y);
      context.lineTo(width - padding.right, y);
      context.stroke();
    }

    if (history.length === 0) {
      context.textAlign = 'center';
      context.fillText('waiting for the first epoch to close…', width / 2, height / 2);
      return;
    }

    // Grow the axis in steps rather than showing 400 epochs of empty space
    // from the first one, but never past the run length.
    const totalEpochs = Math.min(
      Math.max(snapshot.settings.training_epochs, history.length),
      Math.max(50, Math.ceil(history.length / 50) * 50 + 50),
    );
    const maxLoss = Math.max(...history.map((point) => point.loss), 0.1);
    const xFor = (epoch) => padding.left + (plotWidth * (epoch - 1)) / Math.max(totalEpochs - 1, 1);
    const yForLoss = (loss) => padding.top + plotHeight * (1 - Math.min(loss / maxLoss, 1));
    const yForAccuracy = (accuracy) => padding.top + plotHeight * (1 - accuracy);

    for (const checkpoint of snapshot.checkpoints) {
      const x = xFor(checkpoint.epoch);
      context.strokeStyle = 'rgba(167, 139, 250, 0.35)';
      context.beginPath();
      context.moveTo(x, padding.top);
      context.lineTo(x, padding.top + plotHeight);
      context.stroke();
    }

    const line = (points, color) => {
      context.strokeStyle = color;
      context.lineWidth = 1.6;
      context.beginPath();
      points.forEach(([x, y], index) => {
        if (index === 0) {
          context.moveTo(x, y);
        } else {
          context.lineTo(x, y);
        }
      });
      context.stroke();
    };

    line(
      history.map((point) => [xFor(point.epoch), yForLoss(point.loss)]),
      LOSS_COLOR,
    );
    const accuracyPoints = history
      .filter((point) => point.accuracy !== null)
      .map((point) => [xFor(point.epoch), yForAccuracy(point.accuracy)]);
    if (accuracyPoints.length > 0) {
      line(accuracyPoints, ACCURACY_COLOR);
    }

    context.fillStyle = TEXT_COLOR;
    context.textAlign = 'right';
    context.fillText(maxLoss.toFixed(2), padding.left - 6, padding.top + 4);
    context.fillText('0', padding.left - 6, padding.top + plotHeight + 4);
    context.textAlign = 'left';
    context.fillStyle = ACCURACY_COLOR;
    context.fillText('100%', width - padding.right + 6, padding.top + 4);
    context.fillText('0%', width - padding.right + 6, padding.top + plotHeight + 4);

    context.fillStyle = TEXT_COLOR;
    context.textAlign = 'center';
    context.fillText(`epoch ${history[history.length - 1].epoch} of ${totalEpochs}`, width / 2, height - 6);

    const last = history[history.length - 1];
    context.fillStyle = LOSS_COLOR;
    context.beginPath();
    context.arc(xFor(last.epoch), yForLoss(last.loss), 2.6, 0, Math.PI * 2);
    context.fill();
    if (last.accuracy !== null) {
      context.fillStyle = ACCURACY_COLOR;
      context.beginPath();
      context.arc(xFor(last.epoch), yForAccuracy(last.accuracy), 2.6, 0, Math.PI * 2);
      context.fill();
    }
  }
}

/**
 * The decision surface over the square the samples are drawn from, with the
 * data on top: filled dots for the training set, ringed dots for the held-out
 * fifth that no worker ever computes a gradient on.
 */
export class BoundaryView {
  constructor(canvas, { resolution = 96 } = {}) {
    this.canvas = canvas;
    this.resolution = resolution;
    this.field = document.createElement('canvas');
    this.field.width = resolution;
    this.field.height = resolution;
    this.fieldContext = this.field.getContext('2d');
    this.image = this.fieldContext.createImageData(resolution, resolution);
    this.lastDrawnEpoch = -1;
  }

  /** Recomputes the surface. Cheap enough to do on every epoch, not every frame. */
  computeField(spec, parameters) {
    const size = this.resolution;
    const data = this.image.data;
    const hidden = new Float32Array(spec.hidden);
    const probabilities = new Float32Array(spec.outputs);
    const features = new Float32Array(2);

    for (let row = 0; row < size; row += 1) {
      features[1] = 1 - (2 * (row + 0.5)) / size;
      for (let column = 0; column < size; column += 1) {
        features[0] = (2 * (column + 0.5)) / size - 1;
        model.forward(spec, parameters, features, hidden, probabilities);

        const inside = probabilities[1];
        const confidence = Math.abs(inside - 0.5) * 2;
        const offset = (row * size + column) * 4;
        // Teal where the model says "inside the circle", rose where it says
        // "outside"; pale where it is unsure, which is where the boundary is.
        const alpha = 26 + confidence * 96;
        if (inside >= 0.5) {
          data[offset] = 45;
          data[offset + 1] = 212;
          data[offset + 2] = 191;
        } else {
          data[offset] = 251;
          data[offset + 1] = 113;
          data[offset + 2] = 133;
        }
        data[offset + 3] = alpha;
      }
    }

    this.fieldContext.putImageData(this.image, 0, 0);
  }

  draw(snapshot, simulation) {
    const { context, width, height } = prepare(this.canvas);
    const size = Math.min(width, height) - 16;
    const left = (width - size) / 2;
    const top = (height - size) / 2;

    if (this.lastDrawnEpoch !== snapshot.epochsCompleted) {
      this.computeField(simulation.spec, simulation.state.parameters);
      this.lastDrawnEpoch = snapshot.epochsCompleted;
    }

    context.save();
    context.beginPath();
    context.rect(left, top, size, size);
    context.clip();
    context.fillStyle = '#0a101c';
    context.fillRect(left, top, size, size);
    context.imageSmoothingEnabled = true;
    context.drawImage(this.field, left, top, size, size);

    const toCanvas = (x, y) => [left + ((x + 1) / 2) * size, top + ((1 - y) / 2) * size];

    // The true boundary, for comparison with what the network drew.
    context.strokeStyle = 'rgba(230, 236, 247, 0.4)';
    context.setLineDash([4, 4]);
    context.lineWidth = 1;
    context.beginPath();
    context.arc(left + size / 2, top + size / 2, (RADIUS * size) / 2, 0, Math.PI * 2);
    context.stroke();
    context.setLineDash([]);

    const drawPoints = (samples, radius, ringed) => {
      for (const sample of samples) {
        const [x, y] = toCanvas(sample.features[0], sample.features[1]);
        context.beginPath();
        context.arc(x, y, radius, 0, Math.PI * 2);
        if (ringed) {
          context.strokeStyle = sample.label === 1 ? '#5eead4' : '#fda4af';
          context.lineWidth = 1.2;
          context.stroke();
        } else {
          context.fillStyle = sample.label === 1 ? 'rgba(94, 234, 212, 0.75)' : 'rgba(253, 164, 175, 0.75)';
          context.fill();
        }
      }
    };

    drawPoints(simulation.trainingSet, 1.4, false);
    drawPoints(simulation.validationSet, 2.6, true);

    context.restore();

    context.strokeStyle = 'rgba(148, 163, 184, 0.25)';
    context.strokeRect(left + 0.5, top + 0.5, size - 1, size - 1);
  }
}
