import wasm_init, {
    setup,
    DesktopSize,
    DeviceEvent,
    InputTransaction,
    SessionBuilder,
    ClipboardData,
    Extension,
    RdpFile,
} from '../../../crates/ironrdp-web/pkg/ironrdp_web';

export async function init(log_level: string) {
    await wasm_init();
    setup(log_level);
}

export { RdpFile };

export const Backend = {
    DesktopSize: DesktopSize,
    InputTransaction: InputTransaction,
    SessionBuilder: SessionBuilder,
    ClipboardData: ClipboardData,
    DeviceEvent: DeviceEvent,
};

export function preConnectionBlob(pcb: string): Extension {
    return new Extension('pcb', pcb);
}

export function displayControl(enable: boolean): Extension {
    return new Extension('display_control', enable);
}

export function kdcProxyUrl(url: string): Extension {
    return new Extension('kdc_proxy_url', url);
}

export function outboundMessageSizeLimit(limit: number): Extension {
    return new Extension('outbound_message_size_limit', limit);
}

export function enableCredssp(enable: boolean): Extension {
    return new Extension('enable_credssp', enable);
}

/**
 * Enable or disable audio playback for the RDP session.
 *
 * Requires a user gesture (click/touch/keypress) before audio will play
 * due to browser autoplay policy.
 */
export function enableAudio(enable: boolean): Extension {
    return new Extension('enable_audio', enable);
}

/**
 * Set the preferred sample rate for audio format negotiation (e.g. 48000).
 *
 * Defaults to the browser's native rate. The client handles conversion
 * automatically if the server picks a different rate.
 */
export function audioSampleRate(rate: number): Extension {
    return new Extension('audio_sample_rate', rate);
}
