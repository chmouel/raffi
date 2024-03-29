# typed: false
# frozen_string_literal: true

# This file was generated by GoReleaser. DO NOT EDIT.
class Raffi < Formula
  desc "raffi - fuzzel launcher based on yaml configuration"
  homepage "https://github.com/chmouel/raffi"
  version "0.4.0"
  depends_on :linux

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/chmouel/raffi/releases/download/v0.4.0/raffi_0.4.0_linux_x86_64.tar.gz"
      sha256 "93857f3a5e3590f605e3098ffe882d77f6d8a10ba205dc711771d0806c3e6fdc"

      def install
        bin.install "raffi" => "raffi"
        prefix.install_metafiles
      end
    end
  end
end
