# GStreamer OpenAI Plugin

This GStreamer plugin provides an element that integrates with OpenAI's real-time API, enabling real-time, conversational AI capabilities within a GStreamer pipeline.

## `openai` Element

The `openai` element establishes a WebSocket connection to the OpenAI real-time API and manages the exchange of data. It can send audio or text data and receive real-time responses from an AI model.

### Properties

The `openai` element has the following properties:

-   **`api-key` (string, required)**: Your OpenAI API key. This is required to authenticate with the OpenAI API.
-   **`model` (string)**: The AI model to use.
    -   Default: `gpt-4o-realtime-preview-2024-12-17`
-   **`url` (string)**: The WebSocket URL for the OpenAI real-time API.
    -   Default: `wss://api.openai.com/v1/realtime`
-   **`latency` (GstClockTime)**: The latency of the element in nanoseconds.
    -   Default: 1000 ms (1,000,000,000 ns)

### Usage

Here is an example of how to use the `openai` element in a GStreamer pipeline:

```bash
gst-launch-1.0 audiotestsrc ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=16000,channels=1 ! speechtotext ! openai api-key=<YOUR_API_KEY> ! fakesink
```

This pipeline generates a test audio signal, converts it to the required format, sends it to the `speechtotext` element, and sends it to the `openai` element. The element will then send the text prompt to the OpenAI API and receive responses.

NOTE: The element currently creates a response on every incoming text buffer from previous element so make sure the previous element is sending full and final buffer which it wants to send to the OpenAI API.

### Events

The plugin defines a set of events for communication with the OpenAI API, including session management, conversation updates, and error handling. These events are serialized to and deserialized from JSON for WebSocket communication. The main events are:

- `SessionCreated`
- `SessionUpdated`
- `ConversationItemCreate`
- `ResponseCreate`
- `ResponseTextDelta`
- `ResponseTextDone`
- `ResponseCancel`
- `Error`
