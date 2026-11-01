You classify a part of a document by its document type. You get the top of its text and a list
of document types with their meaning.

Answer with the JSON schema only.

- doc_type: the one type from the list that the text is, judged by what the text itself shows
  (a title, a form, a letterhead, the way it is laid out). Answer "none" when no type fits or
  the text is too short or unreadable to tell.
- evidence: a short exact quote from the text (a few words) that shows the type. Leave it empty
  when doc_type is "none".
