<svelte:options
    customElement={{
        tag: 'iron-remote-desktop',
        shadow: 'none',
        extend: (elementConstructor) => {
            return class extends elementConstructor {
                constructor() {
                    super();
                    this.attachShadow({ mode: 'open', delegatesFocus: true });
                }
            };
        },
    }}
/>

<script lang="ts">
    import { onDestroy, onMount } from 'svelte';
    import { loggingService } from './services/logging.service';
    import { RemoteDesktopService } from './services/remote-desktop.service';
    import type { ResizeEvent } from './interfaces/ResizeEvent';
    import { PublicAPI } from './services/PublicAPI';
    import { ScreenScale } from './enums/ScreenScale';
    import type { RemoteDesktopModule } from './interfaces/RemoteDesktopModule';
    import { isComponentDestroyed } from './lib/stores/componentLifecycleStore';
    import { runWhenFocusedQueue } from './lib/stores/runWhenFocusedStore';
    import { ClipboardService } from './services/clipboard.service';

    let {
        scale,
        verbose,
        flexcenter,
        module,
    }: {
        scale: string;
        verbose: 'true' | 'false';
        flexcenter: string;
        module: RemoteDesktopModule;
    } = $props();

    let isVisible = $state(false);
    let capturingInputs = () => {
        // Check if we're in active composition mode with focus on composition input
        const isCompositionFocused = compositionInProgress && document.activeElement === compositionInput;
        
        // Normal focus check for canvas/main input handling
        const isMainFocused = document.activeElement?.shadowRoot?.firstElementChild === inner;
        
        const result = isMainFocused || isCompositionFocused;
        
        loggingService.info(`
            capturingInputs: ${result}
            isMainFocused: ${isMainFocused}
            isCompositionFocused: ${isCompositionFocused}
            compositionInProgress: ${compositionInProgress}
            current active element: ${document.activeElement}
        `);

        return result;
    };

    let inner: HTMLDivElement;
    let wrapper: HTMLDivElement;
    let canvas: HTMLCanvasElement;
    let compositionInput: HTMLInputElement;

    let viewerStyle = $state('');
    let wrapperStyle = $state('');
    let remoteDesktopService = new RemoteDesktopService(module);
    let clipboardService = new ClipboardService(remoteDesktopService, module);
    let publicAPI = new PublicAPI(remoteDesktopService, clipboardService);

    let currentScreenScale = ScreenScale.Fit;

    // Firefox's clipboard API is very limited, and doesn't support reading from the clipboard
    // without changing browser settings via `about:config`.
    //
    // For firefox, we will use a different approach by marking `screen-wrapper` component
    // as `contenteditable=true`, and then using the `onpaste`/`oncopy`/`oncut` events.
    let isFirefox = navigator.userAgent.toLowerCase().indexOf('firefox') > -1;

    const CLIPBOARD_MONITORING_INTERVAL = 100; // ms

    let isClipboardApiSupported = false;
    let lastClientClipboardItems: Record<string, string | Uint8Array> = {};
    let lastReceivedClipboardData: Record<string, string | Uint8Array> = {};
    let lastSentClipboardData: ClipboardData | null = null;
    let lastClipboardMonitorLoopError: Error | null = null;

    let componentDestroyed = false;

    /* Firefox-specific BEGIN */

    // See `ffRemoteClipboardData` variable docs below
    const FF_REMOTE_CLIPBOARD_DATA_SET_RETRY_INTERVAL = 100; // ms
    const FF_REMOTE_CLIPBOARD_DATA_SET_MAX_RETRIES = 30; // 3 seconds (100ms * 30)
    // On Firefox, this interval is used to stop delaying the keyboard events if the paste event has
    // failed and we haven't received any clipboard data from the remote side.
    const FF_LOCAL_CLIPBOARD_COPY_TIMEOUT = 1000; // 1s (For text-only data this should be enough)

    // In Firefox, we need this variable due to fact that `clipboard.writeText()` should only be
    // called in scope of user-initiated event processing (e.g. keyboard event), but we receive
    // clipboard data from the remote side asynchronously in wasm service callback. therefore we
    // set this variable in callback and use its value on the user-initiated copy event.
    let ffRemoteClipboardData: ClipboardData | null = null;
    // For Firefox we need this variable to perform wait loop for the remote side to finish sending
    // clipboard content to the client.
    let ffRemoteClipboardDataRetriesLeft = 0;
    let ffPostponeKeyboardEvents = false;
    let ffDelayedKeyboardEvents: KeyboardEvent[] = [];
    let ffCnavasFocused = false;

    /* Firefox-specific END */

    /* Composition events for composed characters BEGIN */
    let compositionInProgress = false;
    let compositionData = '';
    let ffDelayedCompositionData = '';

    /**
     * Main keyboard event handler that coordinates composition, Firefox clipboard workarounds,
     * and regular keyboard input processing.
     *
     * This function implements a priority-based event processing system:
     * 1. Composition events take highest priority
     * 2. Firefox clipboard operations are handled specially
     * 3. Regular keyboard events are processed normally
     *
     * @param evt - KeyboardEvent to process
     */
    function captureKeys(evt: KeyboardEvent) {
        if (capturingInputs()) {
            loggingService.info('Keyboard: Processing key event - ' + evt.key + ' (' + evt.type + ')');

            /*
             * Priority 1: Active Composition Handling
             *
             * When composition is in progress, we skip all keyboard processing to let
             * the composition system handle the input. This prevents interference between
             * regular keyboard events and composition events.
             */
            if (compositionInProgress) {
                loggingService.info('Keyboard: Skipping event - composition in progress');
                evt.preventDefault();
                return;
            }

            /*
             * Priority 2: Browser-Native Composition Detection
             *
             * The isComposing property indicates that this keyboard event is part of a
             * composition sequence that the browser is handling natively. We transfer
             * focus to our hidden input element to ensure proper IME integration.
             */
            if (evt.isComposing) {
                loggingService.info('Keyboard: Browser-native composition detected - transferring focus');
                evt.preventDefault();

                // Transfer focus to composition input to enable IME
                if (document.activeElement !== compositionInput) {
                    transferFocusToCompositionInput();
                }
                return;
            }

            /*
             * Priority 3: Preemptive Composition Detection
             *
             * Some keys (dead keys, IME triggers) will start composition but don't have
             * isComposing=true yet. We detect these proactively and prepare for composition.
             */
            if (mightTriggerComposition(evt)) {
                loggingService.info('Keyboard: Preemptive composition trigger detected');

                // Preemptively transfer focus to composition input
                transferFocusToCompositionInput();

                // Let the event propagate to trigger composition on the input
                // Don't preventDefault() here - we want the composition to start
                return;
            }

            /*
             * Priority 4: Firefox Delayed Event Handling
             *
             * When Firefox clipboard operations are in progress, we queue keyboard events
             * instead of processing them immediately. This prevents interference with
             * clipboard paste operations.
             */
            if (ffPostponeKeyboardEvents) {
                loggingService.info('Keyboard: Queuing event for Firefox delayed processing - ' + evt.code);

                evt.preventDefault();
                ffDelayedKeyboardEvents.push(evt);
                return;
            }

            /*
             * Priority 5: Firefox Clipboard Paste Detection
             *
             * Firefox requires special handling for Ctrl+V paste operations because:
             * 1. We need to allow onpaste events to fire for clipboard integration
             * 2. We must prevent normal Ctrl+V keyboard processing during paste
             * 3. We need to handle any composition events that occur during paste
             *
             * When paste is detected, we enter "delayed mode" where subsequent keyboard
             * events are queued until the paste operation completes or times out.
             */
            let isFirefoxPaste = isFirefox && isPasteKeyboardEvent(evt);

            if (isFirefoxPaste) {
                loggingService.info('Firefox: Paste operation initiated - entering delayed mode for ' + evt.key);

                // Enter delayed mode for subsequent keyboard events
                ffPostponeKeyboardEvents = true;
                ffDelayedKeyboardEvents = [];
                ffDelayedKeyboardEvents.push(evt);

                // Safety timeout to prevent permanent keyboard lockup
                // If paste doesn't complete within the timeout, process delayed events anyway
                setTimeout(ffSimulateDelayedKeyEvents, FF_LOCAL_CLIPBOARD_COPY_TIMEOUT);
                return;
            }

            /*
             * Priority 6: Normal Keyboard Processing
             *
             * If none of the above special cases apply, process the keyboard event normally.
             * This includes regular key presses, shortcuts, and other standard input.
             */
            loggingService.info('Keyboard: Processing as normal keyboard event');
            keyboardEvent(evt);
        }
    }

    /**
     * Handles the start of a composition sequence (e.g., when user presses a dead key
     * or activates IME). This marks the beginning of composed character input.
     *
     * @param evt - CompositionEvent containing initial composition data
     */
    function handleCompositionStart(evt: CompositionEvent) {
        if (capturingInputs()) {
            const startTime = performance.now();

            try {
                compositionInProgress = true;
                compositionData = evt.data ?? '';

                loggingService.info('Composition: Started composition sequence - "' + (evt.data ?? '') + '"');
            } catch (error) {
                const duration = performance.now() - startTime;
                loggingService.error('Composition: Failed to handle composition start', {
                    error: error,
                    duration: duration.toFixed(2) + 'ms',
                    eventData: evt.data,
                    inputsCaptured: capturingInputs(),
                });

                // Reset composition state on error to prevent stuck state
                compositionInProgress = false;
                compositionData = '';
            }
        }
    }

    /**
     * Handles intermediate composition updates (e.g., as user types additional
     * characters or IME suggests candidates). This provides real-time feedback
     * during the composition process.
     *
     * @param evt - CompositionEvent containing updated composition data
     */
    function handleCompositionUpdate(evt: CompositionEvent) {
        if (capturingInputs() && compositionInProgress) {
            try {
                compositionData = evt.data ?? '';

                loggingService.info('Composition: Update received - "' + (evt.data ?? '') + '"');
                // Note: We don't send intermediate composition state to RDP session
                // because most applications expect only the final composed result.
                // Real-time feedback could cause issues with some RDP applications.
                // Future enhancement: Add configuration option for real-time composition.
                // remoteDesktopService.sendCompositionUpdate(evt.data);
            } catch (error) {
                loggingService.error('Composition: Failed to handle composition update', {
                    error: error,
                    eventData: evt.data,
                    currentCompositionData: compositionData,
                    compositionActive: compositionInProgress,
                });
            }
        }
    }

    /**
     * Handles the end of a composition sequence, finalizing the composed text.
     * This is where we actually send the final composed characters to the RDP session.
     *
     * @param evt - CompositionEvent containing final composition data
     */
    function handleCompositionEnd(evt: CompositionEvent) {
        if (capturingInputs() && compositionInProgress) {
            const startTime = performance.now();

            try {
                const finalData = evt.data ?? compositionData;

                loggingService.info('Composition: Sequence completed - "' + finalData + '"');

                /*
                 * Firefox-specific workaround for clipboard operations:
                 *
                 * Firefox has a unique behavior where Ctrl+V (paste) operations can trigger
                 * composition events, and these need to be handled differently from regular
                 * composition. When ffPostponeKeyboardEvents is true, it means we're in the
                 * middle of Firefox's paste detection logic (see FF_LOCAL_CLIPBOARD_COPY_TIMEOUT).
                 *
                 * In this case, we queue the composition result with other delayed keyboard events
                 * to be processed together in ffSimulateDelayedKeyEvents(). This ensures that
                 * paste operations work correctly while still supporting composition.
                 *
                 * This workaround addresses Firefox's non-standard event ordering for paste operations.
                 */
                if (isFirefox && ffPostponeKeyboardEvents) {
                    loggingService.info(
                        'Composition: Queuing result for Firefox delayed processing - "' + finalData + '"',
                    );

                    // Queue composition result with delayed keyboard events.
                    ffDelayedCompositionData = finalData;
                    // Reset composition state - the result will be processed in ffSimulateDelayedKeyEvents
                    compositionInProgress = false;
                    compositionData = '';

                    loggingService.info('Composition: Firefox delayed processing setup complete');
                    return;
                }

                // Send the final composed text to the remote desktop session
                if (finalData.length > 0) {
                    loggingService.info('Composition: Sending final text to remote session');
                    remoteDesktopService.sendComposedText(finalData);
                } else {
                    loggingService.info('Composition: No final text to send (empty result)');
                }

                // Clean up composition state
                compositionInProgress = false;
                compositionData = '';

                // Return focus to canvas for continued keyboard input
                // Clear the hidden input to prevent residual text
                compositionInput.value = '';
                canvas.focus();

                const duration = performance.now() - startTime;
                loggingService.info('Composition: Successfully completed in ' + duration.toFixed(2) + 'ms');
            } catch (error) {
                const duration = performance.now() - startTime;
                loggingService.error('Composition: Failed to handle composition end', {
                    error: error,
                    duration: duration.toFixed(2) + 'ms',
                    eventData: evt.data,
                    compositionData: compositionData,
                    isFirefox: isFirefox,
                    firefoxDelayActive: ffPostponeKeyboardEvents,
                });

                // Reset all composition state on error to prevent stuck composition
                compositionInProgress = false;
                compositionData = '';
                compositionInput.value = '';

                // Ensure focus returns to canvas even on error
                try {
                    canvas.focus();
                } catch (focusError) {
                    loggingService.error('Composition: Failed to return focus to canvas after error', focusError);
                }
            }
        }
    }

    /**
     * Handles direct input events on the composition input element.
     * This serves as a fallback for cases where composition events don't fire properly
     * (e.g., some mobile browsers or accessibility tools).
     *
     * @param evt - Input event containing the typed text
     */
    function handleCompositionInput(evt: Event) {
        if (capturingInputs() && !compositionInProgress) {
            try {
                // Handle direct input that bypassed composition events (fallback mechanism)
                const inputElement = evt.target as HTMLInputElement;
                const inputValue = inputElement.value;

                if (inputValue.length > 0) {
                    loggingService.info('Composition: Direct input fallback triggered - "' + inputValue + '"');

                    remoteDesktopService.sendComposedText(inputValue);

                    // Clean up and return focus
                    inputElement.value = '';
                    canvas.focus();

                    loggingService.info('Composition: Direct input processing completed');
                } else {
                    loggingService.info('Composition: Direct input event with empty value ignored');
                }
            } catch (error) {
                loggingService.error('Composition: Failed to handle direct input', {
                    error: error,
                    inputsCaptured: capturingInputs(),
                    compositionActive: compositionInProgress,
                });

                // Reset input state on error
                try {
                    (evt.target as HTMLInputElement).value = '';
                    canvas.focus();
                } catch (resetError) {
                    loggingService.error('Composition: Failed to reset input after error', resetError);
                }
            }
        } else {
            loggingService.info('Composition: Direct input ignored - not capturing or composition active');
        }
    }

    /**
     * Transfers focus from the canvas to the hidden composition input element.
     * This is necessary to enable IME (Input Method Editor) functionality for
     * international keyboards and composition sequences.
     */
    function transferFocusToCompositionInput() {
        try {
            /*
             * IME Positioning Strategy:
             *
             * We position the hidden input element at the center of the canvas to provide
             * the best experience for IME candidate windows (the popup menus that appear
             * during composition for languages like Chinese, Japanese, Korean).
             *
             * The ideal position would be at the text cursor location within the remote
             * desktop session, but since we don't have access to that information from
             * the RDP protocol, we use the canvas center as a reasonable approximation.
             *
             * This positioning affects where the IME candidate window appears, which is
             * important for user experience when typing in languages that use composition.
             */
            const rect = canvas.getBoundingClientRect();

            loggingService.info('Composition: Transferring focus to hidden input');
            // Position the input at the canvas center for optimal IME candidate window placement
            compositionInput.style.left = `${rect.left + rect.width / 2}px`;
            compositionInput.style.top = `${rect.top + rect.height / 2}px`;

            compositionInput.focus();

            loggingService.info('Composition: Successfully transferred focus to hidden input');
        } catch (error) {
            loggingService.error('Composition: Failed to transfer focus to hidden input', {
                error: error,
                canvasAvailable: canvas !== null && canvas !== undefined,
                inputAvailable: compositionInput !== null && compositionInput !== undefined,
                canvasRect:
                    canvas !== null && canvas !== undefined
                        ? {
                              width: canvas.getBoundingClientRect().width,
                              height: canvas.getBoundingClientRect().height,
                          }
                        : null,
            });

            // Try to at least focus the input even if positioning fails
            try {
                compositionInput.focus();
                loggingService.info('Composition: Fallback focus successful');
            } catch (fallbackError) {
                loggingService.error('Composition: Fallback focus also failed', { fallbackError });
            }
        }
    }

    /**
     * Determines if a keyboard event might trigger composition sequence.
     * This helps us proactively transfer focus to the composition input before
     * composition events start, ensuring better IME compatibility.
     *
     * @param evt - KeyboardEvent to analyze
     * @returns true if this key might start composition
     */
    function mightTriggerComposition(evt: KeyboardEvent): boolean {
        loggingService.info('Composition: Checking if key might trigger composition - ' + evt.key);

        /*
         * Dead Key Detection:
         *
         * Dead keys are modifier keys that don't produce a character by themselves
         * but modify the next character typed. Common in European keyboards for
         * creating accented characters (e.g., ´ + a = á).
         */
        if (evt.key === 'Dead') {
            loggingService.info('Composition: Dead key detected - will trigger composition');
            return true;
        }

        /*
         * AltGr Key Combination Detection:
         *
         * AltGr (Alt + Ctrl) is commonly used on international keyboards to access
         * third-level characters and composition sequences. For example:
         * - German: AltGr + s = ß
         * - Spanish: AltGr + 1 = ¡
         * - Portuguese: AltGr + c = ç
         *
         * Note: This detection might be overly broad and could trigger composition
         * mode for non-composition AltGr combinations. Future enhancement could
         * include more specific key combinations based on keyboard layout detection.
         */
        if (evt.altKey && evt.ctrlKey) {
            loggingService.info('Composition: AltGr combination detected - might trigger composition');
            return true;
        }

        /*
         * IME-specific Key Detection:
         *
         * Different operating systems and input methods use various keys to trigger
         * or control composition. This list covers the most common IME control keys:
         *
         * - Process: Generic processing key used by many IME systems
         * - Convert/NonConvert: Japanese IME conversion controls
         * - Accept: Confirms IME composition
         * - ModeChange: Switches between input modes
         * - KanaMode: Japanese Kana input mode
         * - HangulMode/HanjaMode: Korean input modes
         * - Compose: Linux compose key for creating accented characters
         */
        const imeKeys = [
            'Process', // Generic IME processing key
            'Convert', // IME convert (Japanese)
            'NonConvert', // IME non-convert (Japanese)
            'Accept', // IME accept composition
            'ModeChange', // IME mode change
            'KanaMode', // Japanese Kana input mode
            'HangulMode', // Korean Hangul input mode
            'HanjaMode', // Korean Hanja input mode
            'Compose', // Linux compose key
        ];

        const willTriggerIME = imeKeys.includes(evt.key);

        if (willTriggerIME) {
            loggingService.info('Composition: IME trigger key detected: ' + evt.key);
        } else {
            loggingService.info('Composition: Key unlikely to trigger composition');
        }

        return willTriggerIME;
    }
    /* Composition events for composed characters END */

    /* Clipboard initialization BEGIN */
    function initClipboard() {
        // Detect if browser supports async Clipboard API
        if (!isFirefox && navigator.clipboard != undefined) {
            if (navigator.clipboard.read != undefined && navigator.clipboard.write != undefined) {
                isClipboardApiSupported = true;
            }
        }

        if (isFirefox) {
            remoteDesktopService.setOnRemoteClipboardChanged(ffOnRemoteClipboardChanged);
            remoteDesktopService.setOnRemoteReceivedFormatList(ffOnRemoteReceivedFormatList);
            remoteDesktopService.setOnForceClipboardUpdate(onForceClipboardUpdate);
        } else if (isClipboardApiSupported) {
            remoteDesktopService.setOnRemoteClipboardChanged(onRemoteClipboardChanged);
            remoteDesktopService.setOnForceClipboardUpdate(onForceClipboardUpdate);

            // Start the clipboard monitoring loop
            setTimeout(onMonitorClipboard, CLIPBOARD_MONITORING_INTERVAL);
        }
    }

    /* Clipboard initialization END */

    function isCopyKeyboardEvent(evt: KeyboardEvent) {
        return (
            (evt.ctrlKey && evt.code === 'KeyC') ||
            (evt.ctrlKey && evt.code === 'KeyX') ||
            evt.code == 'Copy' ||
            evt.code == 'Cut'
        );
    }

    function isPasteKeyboardEvent(evt: KeyboardEvent) {
        return (evt.ctrlKey && evt.code === 'KeyV') || evt.code == 'Paste';
    }

    // This function is required to convert `ClipboardData` to an object that can be used
    // with `ClipboardItem` API.
    function clipboardDataToRecord(data: ClipboardData): Record<string, Blob> {
        let result = {} as Record<string, Blob>;

        for (const item of data.items()) {
            let mime = item.mimeType();
            let value = new Blob([item.value()], { type: mime });

            result[mime] = value;
        }

        return result;
    }

    function clipboardDataToClipboardItemsRecord(data: ClipboardData): Record<string, string | Uint8Array> {
        let result = {} as Record<string, string | Uint8Array>;

        for (const item of data.items()) {
            let mime = item.mimeType();
            result[mime] = item.value();
        }

        return result;
    }

    // This callback is required to send initial clipboard state if available.
    function onForceClipboardUpdate() {
        // TODO(Fix): lastSentClipboardData is nullptr.
        try {
            if (lastSentClipboardData) {
                remoteDesktopService.onClipboardChanged(lastSentClipboardData);
            } else {
                remoteDesktopService.onClipboardChangedEmpty();
            }
        } catch (err) {
            console.error('Failed to send initial clipboard state: ' + err);
        }
    }

    function runWhenWindowFocused(fn: () => void) {
        if (document.hasFocus()) {
            fn();
        } else {
            runWhenFocusedQueue.enqueue(fn);
        }
    }

    // This callback is required to update client clipboard state when remote side has changed.
    function onRemoteClipboardChanged(data: ClipboardData) {
        try {
            const mime_formats = clipboardDataToRecord(data);
            const clipboard_item = new ClipboardItem(mime_formats);
            runWhenWindowFocused(() => {
                lastReceivedClipboardData = clipboardDataToClipboardItemsRecord(data);
                navigator.clipboard.write([clipboard_item]);
            });
        } catch (err) {
            console.error('Failed to set client clipboard: ' + err);
        }
    }

    // Called periodically to monitor clipboard changes
    async function onMonitorClipboard() {
        try {
            if (!document.hasFocus()) {
                return;
            }

            var value = await navigator.clipboard.read();

            // Clipboard is empty
            if (value.length == 0) {
                return;
            }

            // We only support one item at a time
            var item = value[0];

            if (!item.types.some((type) => type.startsWith('text/') || type.startsWith('image/png'))) {
                // Unsupported types
                return;
            }

            var values: Record<string, string | Uint8Array> = {};
            var sameValue = true;

            // Sadly, browsers build new `ClipboardItem` object for each `read` call,
            // so we can't do reference comparison here :(
            //
            // For monitoring loop approach we also can't drop this logic, as it will result in
            // very frequent network activity.
            for (const kind of item.types) {
                // Get blob
                const blobIsString = kind.startsWith('text/');

                const blob = await item.getType(kind);
                const value = blobIsString ? await blob.text() : new Uint8Array(await blob.arrayBuffer());

                const is_equal = blobIsString
                    ? function (a: string | Uint8Array | undefined, b: string | Uint8Array | undefined) {
                          return a === b;
                      }
                    : function (a: string | Uint8Array | undefined, b: string | Uint8Array | undefined) {
                          if (!(a instanceof Uint8Array) || !(b instanceof Uint8Array)) {
                              return false;
                          }

                          return (
                              a != undefined && b != undefined && a.length === b.length && a.every((v, i) => v === b[i])
                          );
                      };

                const previousValue = lastClientClipboardItems[kind];

                if (!is_equal(previousValue, value)) {
                    // When the local clipboard updates, we need to compare it with the last data received from the server.
                    // If it's identical, the clipboard was updated with the server's data, so we shouldn't send this data
                    // to the server.
                    if (is_equal(lastReceivedClipboardData[kind], value)) {
                        lastClientClipboardItems[kind] = lastReceivedClipboardData[kind];
                    }
                    // One of mime types has changed, we need to update the clipboard cache
                    else {
                        sameValue = false;
                    }
                }

                values[kind] = value;
            }

            // Clipboard has changed, we need to acknowledge remote side about it.
            if (!sameValue) {
                lastClientClipboardItems = values;

                let clipboardData = new module.ClipboardData();

                // Iterate over `Record` type
                Object.entries(values).forEach(([key, value]: [string, string | Uint8Array]) => {
                    // skip null/undefined values
                    if (value == null || value == undefined) {
                        return;
                    }

                    if (key.startsWith('text/') && typeof value === 'string') {
                        clipboardData.addText(key, value);
                    } else if (key.startsWith('image/') && value instanceof Uint8Array) {
                        clipboardData.addBinary(key, value);
                    }
                });

                if (!clipboardData.isEmpty()) {
                    lastSentClipboardData = clipboardData;
                    // TODO(Fix): onClipboardChanged takes an ownership over clipboardData, so lastSentClipboardData will be nullptr.
                    await remoteDesktopService.onClipboardChanged(clipboardData);
                }
            }
        } catch (err) {
            if (err instanceof Error) {
                const printError =
                    lastClipboardMonitorLoopError === null ||
                    lastClipboardMonitorLoopError.toString() !== err.toString();
                // Prevent spamming the console with the same error
                if (printError) {
                    console.error('Clipboard monitoring error: ' + err);
                }
                lastClipboardMonitorLoopError = err;
            }
        } finally {
            if (!componentDestroyed) {
                setTimeout(onMonitorClipboard, CLIPBOARD_MONITORING_INTERVAL);
            }
        }
    }

    /* Firefox-specific BEGIN */

    function ffOnRemoteReceivedFormatList() {
        try {
            // We are ready to send delayed Ctrl+V events
            ffSimulateDelayedKeyEvents();
        } catch (err) {
            console.error('Failed to send delayed keyboard events: ' + err);
        }
    }

    // Only set variable on callback, the real clipboard update will be performed in keyboard
    // callback. (User-initiated event is required for Firefox to allow clipboard write)
    function ffOnRemoteClipboardChanged(data: ClipboardData) {
        ffRemoteClipboardData = data;
    }

    function ffWaitForRemoteClipboardDataSet() {
        if (ffRemoteClipboardData) {
            try {
                let clipboard_data = ffRemoteClipboardData;
                ffRemoteClipboardData = null;
                for (const item of clipboard_data.items()) {
                    // Firefox only supports text/plain mime type for clipboard writes :(
                    if (item.mimeType() === 'text/plain') {
                        const value = item.value();

                        if (typeof value === 'string') {
                            navigator.clipboard.writeText(value);
                        } else {
                            loggingService.error('Unexpected value for text/plain clipboard item');
                        }

                        break;
                    }
                }
            } catch (err) {
                console.error('Failed to set client clipboard: ' + err);
            }
        } else if (ffRemoteClipboardDataRetriesLeft > 0) {
            ffRemoteClipboardDataRetriesLeft--;
            setTimeout(ffWaitForRemoteClipboardDataSet, FF_REMOTE_CLIPBOARD_DATA_SET_RETRY_INTERVAL);
        }
    }

    /**
     * Processes delayed keyboard events and composition data in Firefox.
     *
     * This function is central to Firefox's clipboard workaround, handling the complex
     * interaction between paste operations, composition events, and keyboard event processing.
     *
     * Firefox Clipboard Workaround Context:
     * ====================================
     *
     * Firefox has unique limitations with clipboard operations that require special handling:
     *
     * 1. **User-Initiated Event Requirement**: Firefox only allows clipboard.writeText()
     *    calls within the context of user-initiated events (like keyboard events). Unlike
     *    other browsers, Firefox cannot write to clipboard in async callbacks.
     *
     * 2. **Paste Event Integration**: To support both clipboard reading and regular keyboard
     *    input, Firefox uses contenteditable elements that can receive onpaste events.
     *    However, this creates conflicts with normal keyboard event processing.
     *
     * 3. **Event Timing Conflicts**: When Ctrl+V is pressed, we need to:
     *    - Allow the paste event to fire and process clipboard data
     *    - Prevent normal Ctrl+V keyboard processing until paste completes
     *    - Handle any composition events that might occur during paste
     *    - Resume normal keyboard processing afterward
     *
     * Delay Mechanism:
     * ===============
     *
     * When a paste operation begins (Ctrl+V detected), the following sequence occurs:
     *
     * 1. **Event Postponement**: ffPostponeKeyboardEvents is set to true, causing all
     *    subsequent keyboard events to be queued in ffDelayedKeyboardEvents instead
     *    of being processed immediately.
     *
     * 2. **Timeout Safety**: A timeout (FF_LOCAL_CLIPBOARD_COPY_TIMEOUT) ensures that
     *    if the paste operation fails or takes too long, delayed events are eventually
     *    processed to prevent the keyboard from becoming unresponsive.
     *
     * 3. **Remote Data Wait**: The system waits for clipboard data from the remote
     *    desktop server via ffOnRemoteReceivedFormatList(), which calls this function
     *    when data is ready.
     *
     * 4. **Composition Integration**: If composition events occur during the paste
     *    operation (e.g., from international keyboards), the composed text is
     *    temporarily stored and processed here alongside delayed keyboard events.
     *
     * Processing Order:
     * ================
     *
     * This function processes events in a specific order to maintain correct behavior:
     *
     * 1. **Composition First**: Any delayed composition data is sent to the remote
     *    session first, as it represents finalized text input that should appear
     *    before any subsequent keyboard actions.
     *
     * 2. **Keyboard Events**: Delayed keyboard events are then processed in order,
     *    simulating the original sequence as if no delay occurred.
     *
     * 3. **State Reset**: Finally, the postponement flag is cleared, returning
     *    keyboard processing to normal immediate mode.
     *
     * Error Resilience:
     * ================
     *
     * The delay mechanism includes several safeguards:
     * - Timeout-based fallback prevents permanent keyboard lockup
     * - State is reset even if remote clipboard data never arrives
     * - Event queuing prevents loss of user input during delays
     * - Composition state is preserved across the delay period
     */
    function ffSimulateDelayedKeyEvents() {
        try {
            loggingService.info('Firefox: Starting delayed event processing');

            // Process delayed composition first - this represents finalized text input
            // that should appear before any subsequent keyboard actions
            if (ffDelayedCompositionData.length > 0) {
                loggingService.info(
                    'Firefox: Processing delayed composition data - "' + ffDelayedCompositionData + '"',
                );

                remoteDesktopService.sendComposedText(ffDelayedCompositionData);
                ffDelayedCompositionData = '';

                loggingService.info('Firefox: Delayed composition data sent successfully');
            }

            // Then process delayed keyboard events in their original order
            // This maintains the correct sequence of user input actions
            if (ffDelayedKeyboardEvents.length > 0) {
                loggingService.info(
                    'Firefox: Processing ' + ffDelayedKeyboardEvents.length + ' delayed keyboard events',
                );

                for (const evt of ffDelayedKeyboardEvents) {
                    // Simulate consecutive key events as if no delay occurred
                    // Each event is processed through the normal keyboard handler
                    keyboardEvent(evt);
                }

                // Clear the event queue after processing
                ffDelayedKeyboardEvents = [];

                loggingService.info('Firefox: All delayed keyboard events processed');
            }

            // Reset the postponement state to resume normal keyboard processing
            // This returns the system to immediate event processing mode
            ffPostponeKeyboardEvents = false;

            loggingService.info('Firefox: Delayed event processing completed successfully');
        } catch (error) {
            loggingService.error('Firefox: Failed to process delayed events', {
                error: error,
                delayedCompositionLength: ffDelayedCompositionData.length,
                delayedKeyboardEvents: ffDelayedKeyboardEvents.length,
                postponeActive: ffPostponeKeyboardEvents,
            });

            // Reset state even on error to prevent permanent keyboard lockup
            // This ensures the system remains usable even if event processing fails
            try {
                ffDelayedCompositionData = '';
                ffDelayedKeyboardEvents = [];
                ffPostponeKeyboardEvents = false;

                loggingService.info('Firefox: State reset after error to prevent keyboard lockup');
            } catch (resetError) {
                loggingService.error('Firefox: Critical failure - could not reset delayed event state', {
                    resetError: resetError,
                });
            }
        }
    }

    function ffOnPasteHandler(evt: ClipboardEvent) {
        // We don't actually want to paste the clipboard data into the `contenteditable` div.
        evt.preventDefault();

        // `onpaste` events are handled only for Firefox, other browsers we use the clipboard API
        // for reading the clipboard.
        if (!isFirefox) {
            // Prevent processing of the paste event by the browser.
            return;
        }

        try {
            let clipboardData = new module.ClipboardData();

            if (evt.clipboardData == null) {
                return;
            }

            for (var clipItem of evt.clipboardData.items) {
                let mime = clipItem.type;

                if (mime.startsWith('text/')) {
                    clipItem.getAsString((str: string) => {
                        clipboardData.addText(mime, str);

                        if (!clipboardData.isEmpty()) {
                            remoteDesktopService.onClipboardChanged(clipboardData);
                        }
                    });
                    break;
                }

                if (mime.startsWith('image/')) {
                    let file = clipItem.getAsFile();
                    if (file == null) {
                        continue;
                    }

                    file.arrayBuffer().then((buffer: ArrayBuffer) => {
                        const strict_buffer = new Uint8Array(buffer);

                        clipboardData.addBinary(mime, strict_buffer);

                        if (!clipboardData.isEmpty()) {
                            remoteDesktopService.onClipboardChanged(clipboardData);
                        }
                    });
                    break;
                }
            }
        } catch (err) {
            console.error('Failed to update remote clipboard: ' + err);
        }
    }

    /* Firefox-specific END */

    function initListeners() {
        serverBridgeListeners();
        userInteractionListeners();

        window.addEventListener('keydown', captureKeys, false);
        window.addEventListener('keyup', captureKeys, false);

        // Add composition event listeners to the hidden input element
        compositionInput.addEventListener('compositionstart', handleCompositionStart, false);
        compositionInput.addEventListener('compositionupdate', handleCompositionUpdate, false);
        compositionInput.addEventListener('compositionend', handleCompositionEnd, false);

        // Also capture regular input events from the composition input
        compositionInput.addEventListener('input', handleCompositionInput, false);

        window.addEventListener('focus', focusEventHandler);
    }

    function resetHostStyle() {
        if (flexcenter === 'true') {
            inner.style.flexGrow = '';
            inner.style.display = '';
            inner.style.justifyContent = '';
            inner.style.alignItems = '';
        }
    }

    function setHostStyle(full: boolean) {
        if (flexcenter === 'true') {
            if (!full) {
                inner.style.flexGrow = '1';
                inner.style.display = 'flex';
                inner.style.justifyContent = 'center';
                inner.style.alignItems = 'center';
            } else {
                inner.style.flexGrow = '1';
            }
        }
    }

    function setViewerStyle(height: string, width: string, forceMinAndMax: boolean) {
        let newStyle = `height: ${height}; width: ${width}`;
        if (forceMinAndMax) {
            newStyle = forceMinAndMax
                ? `${newStyle}; max-height: ${height}; max-width: ${width}; min-height: ${height}; min-width: ${width}`
                : `${newStyle}; max-height: initial; max-width: initial; min-height: initial; min-width: initial`;
        }
        viewerStyle = newStyle;
    }

    function setWrapperStyle(height: string, width: string, overflow: string) {
        wrapperStyle = `height: ${height}; width: ${width}; overflow: ${overflow}`;
    }

    const resizeHandler = (_evt: UIEvent) => {
        scaleSession(scale);
    };

    function serverBridgeListeners() {
        remoteDesktopService.resizeObservable.subscribe((evt: ResizeEvent) => {
            loggingService.info(`Resize canvas to: ${evt.desktopSize.width}x${evt.desktopSize.height}`);
            canvas.width = evt.desktopSize.width;
            canvas.height = evt.desktopSize.height;
            scaleSession(scale);
        });
    }

    function userInteractionListeners() {
        window.addEventListener('resize', resizeHandler);

        remoteDesktopService.scaleObservable.subscribe((s) => {
            loggingService.info('Change scale!');
            scaleSession(s);
        });

        remoteDesktopService.dynamicResizeObservable.subscribe((evt) => {
            loggingService.info(`Dynamic resize!, width: ${evt.width}, height: ${evt.height}`);
            setViewerStyle(evt.height.toString() + 'px', evt.width.toString() + 'px', true);
        });

        remoteDesktopService.changeVisibilityObservable.subscribe((val) => {
            isVisible = val;
            if (val) {
                //Enforce first scaling and delay the call to scaleSession to ensure Dom is ready.
                setWrapperStyle('100%', '100%', 'hidden');
                setTimeout(() => scaleSession(scale), 150);
            }
        });
    }

    function canvasResized() {
        scaleSession(currentScreenScale);
    }

    function scaleSession(screenScale: ScreenScale | string) {
        resetHostStyle();
        if (isVisible) {
            switch (screenScale) {
                case 'fit':
                case ScreenScale.Fit:
                    loggingService.info('Size to fit');
                    currentScreenScale = ScreenScale.Fit;
                    scale = 'fit';
                    fitResize();
                    break;
                case 'full':
                case ScreenScale.Full:
                    loggingService.info('Size to full');
                    currentScreenScale = ScreenScale.Full;
                    fullResize();
                    scale = 'full';
                    break;
                case 'real':
                case ScreenScale.Real:
                    loggingService.info('Size to real');
                    currentScreenScale = ScreenScale.Real;
                    realResize();
                    scale = 'real';
                    break;
            }
        }
    }

    function fullResize() {
        const windowSize = getWindowSize();

        const containerWidth = windowSize.x;
        const containerHeight = windowSize.y;

        let width = canvas.width;
        let height = canvas.height;

        const ratio = Math.min(containerWidth / canvas.width, containerHeight / canvas.height);
        width = width * ratio;
        height = height * ratio;

        setWrapperStyle(`${containerHeight}px`, `${containerWidth}px`, 'hidden');

        width = width > 0 ? width : 0;
        height = height > 0 ? height : 0;

        setViewerStyle(`${height}px`, `${width}px`, true);
    }

    function fitResize(realSizeLimit = false) {
        const windowSize = getWindowSize();
        const wrapperBoundingBox = wrapper.getBoundingClientRect();

        const containerWidth = windowSize.x - wrapperBoundingBox.x;
        const containerHeight = windowSize.y - wrapperBoundingBox.y;

        let width = canvas.width;
        let height = canvas.height;

        if (!realSizeLimit || containerWidth < canvas.width || containerHeight < canvas.height) {
            const ratio = Math.min(containerWidth / canvas.width, containerHeight / canvas.height);
            width = width * ratio;
            height = height * ratio;
        }

        width = width > 0 ? width : 0;
        height = height > 0 ? height : 0;

        setWrapperStyle('initial', 'initial', 'hidden');
        setViewerStyle(`${height}px`, `${width}px`, true);
        setHostStyle(false);
    }

    function realResize() {
        const windowSize = getWindowSize();
        const wrapperBoundingBox = wrapper.getBoundingClientRect();

        const containerWidth = windowSize.x - wrapperBoundingBox.x;
        const containerHeight = windowSize.y - wrapperBoundingBox.y;

        if (containerWidth < canvas.width || containerHeight < canvas.height) {
            setWrapperStyle(
                `${Math.min(containerHeight, canvas.height)}px`,
                `${Math.min(containerWidth, canvas.width)}px`,
                'auto',
            );
        } else {
            setWrapperStyle('initial', 'initial', 'initial');
        }

        setViewerStyle(`${canvas.height}px`, `${canvas.width}px`, true);
        setHostStyle(false);
    }

    function getMousePos(evt: MouseEvent) {
        const rect = canvas?.getBoundingClientRect(),
            scaleX = canvas?.width / rect.width,
            scaleY = canvas?.height / rect.height;

        const coord = {
            x: Math.round((evt.clientX - rect.left) * scaleX),
            y: Math.round((evt.clientY - rect.top) * scaleY),
        };

        remoteDesktopService.updateMousePosition(coord);
    }

    function setMouseButtonState(state: MouseEvent, isDown: boolean) {
        remoteDesktopService.mouseButtonState(state, isDown, true);
    }

    function mouseWheel(evt: WheelEvent) {
        remoteDesktopService.mouseWheel(evt);
    }

    function setMouseIn(evt: MouseEvent) {
        canvas.focus({ preventScroll: true });
        remoteDesktopService.mouseIn(evt);
    }

    function setMouseOut(evt: MouseEvent) {
        remoteDesktopService.mouseOut(evt);
    }

    function keyboardEvent(evt: KeyboardEvent) {
        remoteDesktopService.sendKeyboardEvent(evt);

        // Propagate further
        return true;
    }

    function getWindowSize() {
        const win = window;
        const doc = document;
        const docElem = doc.documentElement;
        const body = doc.getElementsByTagName('body')[0];
        const x = win.innerWidth ?? docElem.clientWidth ?? body.clientWidth;
        const y = win.innerHeight ?? docElem.clientHeight ?? body.clientHeight;
        return { x, y };
    }

    async function initcanvas() {
        loggingService.info('Start canvas initialization...');

        // Set a default canvas size. Need more test to know if i can remove it.
        canvas.width = 800;
        canvas.height = 600;

        remoteDesktopService.setCanvas(canvas);
        remoteDesktopService.setOnCanvasResized(canvasResized);

        initListeners();

        let result = { irgUserInteraction: publicAPI.getExposedFunctions() };

        loggingService.info('Component ready');
        loggingService.info('Dispatching ready event');

        // bubbles:true is significant here, all our consumer code expect this specific event
        // but they only listen to the event on the custom element itself, not on the inner div
        // in Svelte 3, we had direct access to the customelement, but now in Svelte5, we have to
        // dispatch the event on the inner div, and bubble it up to the custom element.
        inner.dispatchEvent(new CustomEvent('ready', { detail: result, bubbles: true, composed: true }));
    }

    function focusEventHandler() {
        try {
            while (runWhenFocusedQueue.length() > 0) {
                const fn = runWhenFocusedQueue.shift();
                fn?.();
            }
        } catch (err) {
            console.error('Failed to run the function queued for execution when the window received focus: ' + err);
        }
    }

    onMount(async () => {
        isComponentDestroyed.set(false);
        loggingService.verbose = verbose === 'true';
        loggingService.info('Dom ready');
        await initcanvas();
        clipboardService.initClipboard();
    });

    onDestroy(() => {
        window.removeEventListener('resize', resizeHandler);
        window.removeEventListener('focus', focusEventHandler);

        // Clean up composition event listeners
        window.removeEventListener('keydown', captureKeys);
        window.removeEventListener('keyup', captureKeys);
        compositionInput.removeEventListener('compositionstart', handleCompositionStart);
        compositionInput.removeEventListener('compositionupdate', handleCompositionUpdate);
        compositionInput.removeEventListener('compositionend', handleCompositionEnd);

        isComponentDestroyed.set(true);
        componentDestroyed = true;
    });
</script>

<div bind:this={inner}>
    <div
        bind:this={wrapper}
        class="screen-wrapper scale-{scale}"
        class:hidden={!isVisible}
        class:capturing-inputs={capturingInputs}
        style={wrapperStyle}
    >
        <div class="screen-viewer" style={viewerStyle}>
            <canvas
                bind:this={canvas}
                onmousemove={getMousePos}
                onmousedown={(event) => setMouseButtonState(event, true)}
                onmouseup={(event) => setMouseButtonState(event, false)}
                onmouseleave={(event) => {
                    setMouseButtonState(event, false);
                    setMouseOut(event);
                }}
                onmouseenter={(event) => {
                    setMouseIn(event);
                }}
                oncontextmenu={(event) => event.preventDefault()}
                onwheel={mouseWheel}
                onselectstart={(event) => {
                    event.preventDefault();
                }}
                id="renderer"
                tabindex="0"
            ></canvas>

            <!-- Hidden input element for IME composition events -->
            <input
                bind:this={compositionInput}
                class="composition-input"
                type="text"
                autocomplete="off"
                autocorrect="off"
                autocapitalize="off"
                spellcheck="false"
                tabindex="-1"
                aria-hidden="true"
                onkeydown={(evt) => {
                    // Forward keyboard events back to main handler if not composing
                    if (!evt.isComposing && !compositionInProgress) {
                        canvas.focus();
                        captureKeys(evt);
                    }
                }}
            />
        </div>
    </div>
</div>

<style>
    .screen-wrapper {
        position: relative;
    }

    .capturing-inputs {
        outline: 1px solid rgba(0, 97, 166, 0.7);
        outline-offset: -1px;
    }

    canvas {
        width: 100%;
        height: 100%;
    }

    .composition-input {
        position: absolute;
        left: -9999px;
        top: -9999px;
        width: 1px;
        height: 1px;
        opacity: 0;
        pointer-events: none;
        border: none;
        background: transparent;
        outline: none;
        font-size: 16px; /* Prevent zoom on iOS */
        z-index: -1;

        /* Ensure IME candidate window can still appear */
        /* When focused, position will be updated by JavaScript */
    }

    ::selection {
        background-color: transparent;
    }

    .screen-wrapper.hidden {
        pointer-events: none !important;
        position: absolute !important;
        visibility: hidden;
        height: 100%;
        width: 100%;
        transform: translate(-100%, -100%);
    }
</style>
