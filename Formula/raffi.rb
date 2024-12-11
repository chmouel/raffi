# typed: false
# frozen_string_literal: true

# This file was generated by GoReleaser. DO NOT EDIT.
class Raffi < Formula
  desc "raffi - fuzzel launcher based on yaml configuration"
  homepage "https://github.com/chmouel/raffi"
  version "0.8.1"
  depends_on :linux

  on_intel do
    if Hardware::CPU.is_64_bit?
      url "https://github.com/chmouel/raffi/releases/download/v0.8.1/raffi_0.8.1_linux_x86_64.tar.gz"
      sha256 "9a7b35ff688553d67f0d048ebcd1af6d7aaadeba4be1a51f8f9c9263d630a223"

      def install
        bin.install "raffi" => "raffi"
        prefix.install_metafiles
      end
    end
  end
end
