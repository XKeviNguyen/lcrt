// Checks real predictor state, not the stream's diagnostic acknowledgement.
#include <cstdlib>
#include <iostream>
#include <string>
#include <vector>

#include "sherpa-onnx/csrc/online-recognizer.h"
#include "sherpa-onnx/csrc/online-stream.h"
#include "sherpa-onnx/csrc/online-transducer-decoder.h"
#include "sherpa-onnx/csrc/symbol-table.h"

void Require(bool condition, const char *message) {
  if (!condition) {
    std::cerr << message << '\n';
    std::exit(1);
  }
}

int main(int argc, char **argv) {
  Require(argc == 2, "usage: native-context-test model-directory");
  const std::string root = argv[1];
  sherpa_onnx::OnlineRecognizerConfig config;
  const std::string suffix = "-epoch-75-avg-11-chunk-16-left-128";
  config.model_config.transducer.encoder = root + "/encoder" + suffix + ".int8.onnx";
  config.model_config.transducer.decoder = root + "/decoder" + suffix + ".onnx";
  config.model_config.transducer.joiner = root + "/joiner" + suffix + ".int8.onnx";
  config.model_config.tokens = root + "/tokens.txt";
  config.model_config.num_threads = 1;
  config.model_config.provider_config.provider = "cpu";
  config.decoding_method = "greedy_search";
  sherpa_onnx::SymbolTable vocabulary(config.model_config.tokens);
  sherpa_onnx::OnlineRecognizer recognizer(config);
  auto ordinary = recognizer.CreateStream();
  recognizer.IsReady(ordinary.get());
  const auto blank_context = ordinary->GetResult().tokens;
  Require(blank_context.size() >= 1 && blank_context.back() == 0,
          "ordinary recognizer lost default blank seed");
  for (size_t i = 0; i + 1 < blank_context.size(); ++i) {
    Require(blank_context[i] == -1, "predictor padding changed");
  }
  for (const std::string tag : {"<JA>", "<VI>"}) {
    Require(vocabulary.Contains(tag), "language tag absent");
    auto stream = recognizer.CreateStream();
    stream->SetOption("initial_context_token", tag);
    recognizer.IsReady(stream.get());
    auto expected = blank_context;
    expected.back() = vocabulary[tag];
    Require(stream->GetResult().tokens == expected,
            "language token did not reach decoder context");
    Require(recognizer.GetResult(stream.get()).text.empty(),
            "initial language context appeared as caption");
    recognizer.WarmpUpRecognizer(1, 1);
    Require(stream->GetResult().tokens == expected,
            "warmup modified stream context");
    const std::vector<float> silence(16000, 0);
    stream->AcceptWaveform(16000, silence.data(), silence.size());
    while (recognizer.IsReady(stream.get())) {
      recognizer.DecodeStream(stream.get());
    }
    Require(bool(stream->GetResult().decoder_out), "decode did not populate cache");
    // Simulate a completed utterance after the real decode. Reset must replace
    // its predictor context rather than retaining its final lexical tokens.
    stream->GetResult().tokens.push_back(vocabulary[tag]);
    recognizer.Reset(stream.get());
    Require(stream->GetResult().tokens == expected,
            "reset did not restore the language predictor context");
    Require(!stream->GetResult().decoder_out, "reset retained decoder cache");
    stream->AcceptWaveform(16000, silence.data(), silence.size());
    while (recognizer.IsReady(stream.get())) {
      recognizer.DecodeStream(stream.get());
    }
    Require(bool(stream->GetResult().decoder_out),
            "post-reset decode did not rebuild predictor output");
    stream->SetOption("initial_context_token", "");
    Require(!recognizer.IsReady(stream.get()) &&
                !stream->GetOption("initial_context_error").empty(),
            "clearing language after reset was silently accepted");
    recognizer.Reset(stream.get());
    Require(!stream->GetOption("initial_context_error").empty(),
            "reset cleared a terminal configuration error");
  }
  ordinary->GetResult().tokens.push_back(vocabulary["<JA>"]);
  const auto last = ordinary->GetResult().tokens;
  recognizer.Reset(ordinary.get());
  Require(ordinary->GetResult().tokens ==
              std::vector<int64_t>(last.end() - blank_context.size(), last.end()),
          "ordinary reset lost upstream predictor carry behavior");
  std::cout << "native context, warmup, decode cache, reset and default checks passed\n";
}
