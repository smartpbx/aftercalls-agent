import type { CallQuestion } from "./types";

export const QUESTION_TEXT_MAX_CHARS = 2_000;
export const QUESTION_ANSWER_MAX_CHARS = 4_000;
export const NEW_QUESTION_MUTATION_KEY = "__new_question__";

export type QuestionMutationState = {
  /** Changes whenever a question mutation or collection barrier begins/ends.
   * Polls use it to detect a response that crossed a local write. */
  epoch: number;
  nextGeneration: number;
  latestByKey: Readonly<Record<string, number>>;
  collectionGeneration: number | null;
};

export type QuestionMutationToken = {
  callId: string;
  key: string;
  generation: number;
};

export type QuestionCollectionToken = {
  callId: string;
  generation: number;
};

export type QuestionPollToken = {
  callId: string;
  epoch: number;
};

export function createQuestionMutationState(): QuestionMutationState {
  return {
    epoch: 0,
    nextGeneration: 1,
    latestByKey: {},
    collectionGeneration: null,
  };
}

export function beginQuestionMutation(
  state: QuestionMutationState,
  callId: string,
  key: string,
): {
  state: QuestionMutationState;
  token: QuestionMutationToken | null;
} {
  if (
    state.collectionGeneration !== null ||
    state.latestByKey[key] !== undefined
  ) {
    return { state, token: null };
  }

  const generation = state.nextGeneration;
  return {
    state: {
      ...state,
      epoch: state.epoch + 1,
      nextGeneration: generation + 1,
      latestByKey: { ...state.latestByKey, [key]: generation },
    },
    token: { callId, key, generation },
  };
}

export function isQuestionMutationCurrent(
  state: QuestionMutationState,
  token: QuestionMutationToken,
): boolean {
  return state.latestByKey[token.key] === token.generation;
}

export function isQuestionMutationPending(
  state: QuestionMutationState,
  key: string,
): boolean {
  return state.latestByKey[key] !== undefined;
}

export function finishQuestionMutation(
  state: QuestionMutationState,
  token: QuestionMutationToken,
): QuestionMutationState {
  if (!isQuestionMutationCurrent(state, token)) return state;
  const latestByKey = { ...state.latestByKey };
  delete latestByKey[token.key];
  return {
    ...state,
    epoch: state.epoch + 1,
    latestByKey,
  };
}

export function lockQuestionCollection(
  state: QuestionMutationState,
  callId: string,
): {
  state: QuestionMutationState;
  token: QuestionCollectionToken | null;
} {
  if (state.collectionGeneration !== null) return { state, token: null };
  const generation = state.nextGeneration;
  return {
    state: {
      ...state,
      epoch: state.epoch + 1,
      nextGeneration: generation + 1,
      collectionGeneration: generation,
    },
    token: { callId, generation },
  };
}

export function isQuestionCollectionCurrent(
  state: QuestionMutationState,
  token: QuestionCollectionToken,
): boolean {
  return state.collectionGeneration === token.generation;
}

export function unlockQuestionCollection(
  state: QuestionMutationState,
  token: QuestionCollectionToken,
): QuestionMutationState {
  if (!isQuestionCollectionCurrent(state, token)) return state;
  return {
    ...state,
    epoch: state.epoch + 1,
    collectionGeneration: null,
  };
}

export function hasActiveQuestionMutations(
  state: QuestionMutationState,
): boolean {
  return Object.keys(state.latestByKey).length > 0;
}

export function captureQuestionPoll(
  state: QuestionMutationState,
  callId: string,
): QuestionPollToken {
  return { callId, epoch: state.epoch };
}

type CallWithQuestions = {
  id: string;
  questions?: CallQuestion[];
};

/** Merge a poll response without allowing a request that crossed a local
 * question write/barrier to replace the local collection. Non-question fields
 * still advance, so processing status and transcript updates do not stall. */
export function mergeQuestionPoll<T extends CallWithQuestions>(
  current: T,
  fresh: T,
  token: QuestionPollToken,
  state: QuestionMutationState,
): { call: T; questionsPreserved: boolean } {
  if (current.id !== token.callId || fresh.id !== token.callId) {
    return { call: current, questionsPreserved: true };
  }

  const questionsPreserved =
    token.epoch !== state.epoch ||
    state.collectionGeneration !== null ||
    hasActiveQuestionMutations(state);
  if (!questionsPreserved) return { call: fresh, questionsPreserved: false };

  return {
    call: { ...fresh, questions: current.questions },
    questionsPreserved: true,
  };
}

export function replaceQuestionRow(
  questions: readonly CallQuestion[],
  replacement: CallQuestion,
): CallQuestion[] {
  return questions.map((row) =>
    row.id === replacement.id ? replacement : row,
  );
}

/** Roll back only the failed row. Rows added or changed by other requests while
 * this mutation was in flight remain untouched. */
export function rollbackQuestionRow(
  questions: readonly CallQuestion[],
  previous: CallQuestion,
): CallQuestion[] {
  return replaceQuestionRow(questions, previous);
}

export function removeQuestionRow(
  questions: readonly CallQuestion[],
  questionId: string,
): CallQuestion[] {
  return questions.filter((row) => row.id !== questionId);
}

/** Recognise both the current Tauri `other` representation of HTTP 409 and a
 * future structured conflict/status representation without stringifying an
 * arbitrary object. */
export function isQuestionRevisionConflict(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const value = error as {
    kind?: unknown;
    status?: unknown;
    message?: unknown;
  };
  if (value.kind === "conflict" || value.status === 409) return true;
  return (
    value.kind === "other" &&
    typeof value.message === "string" &&
    /\bbackend 409(?:\s+Conflict)?\b/i.test(value.message)
  );
}
