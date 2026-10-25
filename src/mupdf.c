/* The few MuPDF calls farleaf makes, each wrapped in fz_try so an error
 * comes back as a return value (MuPDF reports errors with longjmp, which
 * must not cross into Rust). See src/mupdf.rs for how they are used.
 *
 * Threads: one base context with locks; every thread works through its own
 * clone. A document is used by one thread at a time; display lists can be
 * rendered by many threads at once. */

#include <mupdf/fitz.h>
#include <mupdf/pdf.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>

static pthread_mutex_t mutexes[FZ_LOCK_MAX];
static void lock(void *user, int l) { (void)user; pthread_mutex_lock(&mutexes[l]); }
static void unlock(void *user, int l) { (void)user; pthread_mutex_unlock(&mutexes[l]); }
static fz_locks_context locks = {NULL, lock, unlock};

/* MuPDF prints warnings to stderr, which would scribble over the screen. */
static void quiet(void *user, const char *message) { (void)user; (void)message; }

/* By default MuPDF decodes only the part of an image a draw needs, and
 * caches it under that part: every strip of a scanned page then decodes the
 * scan again (14 times a page). Decoding the whole image once lets every
 * strip use the cached one. Only images that are huge once decoded (over 64
 * million pixels at the scale drawn) keep the default. */
static void whole_images(void *arg, int w, int h, int l2factor, fz_irect *subarea) {
	(void)arg;
	if (((int64_t)w >> l2factor) * ((int64_t)h >> l2factor) <= (int64_t)64 << 20) {
		subarea->x0 = 0;
		subarea->y0 = 0;
		subarea->x1 = w;
		subarea->y1 = h;
	}
}

fz_context *fl_new_context(void) {
	for (int i = 0; i < FZ_LOCK_MAX; i++)
		pthread_mutex_init(&mutexes[i], NULL);
	fz_context *ctx = fz_new_context(NULL, &locks, FZ_STORE_DEFAULT);
	if (!ctx)
		return NULL;
	fz_set_warning_callback(ctx, quiet, NULL);
	fz_set_error_callback(ctx, quiet, NULL);
	fz_tune_image_decode(ctx, whole_images, NULL);
	fz_try(ctx)
		fz_register_document_handlers(ctx);
	fz_catch(ctx) {
		fz_drop_context(ctx);
		return NULL;
	}
	return ctx;
}

fz_context *fl_clone_context(fz_context *base) { return fz_clone_context(base); }

/* Forgets cached decoded images, fonts and such (for timing cold renders). */
void fl_empty_store(fz_context *ctx) { fz_empty_store(ctx); }

/* Opens a PDF held in memory (not copied: keep `data` alive). On error,
 * NULL, with the reason in `err`. */
fz_document *fl_open(fz_context *ctx, const unsigned char *data, size_t len, char *err, int errlen) {
	fz_document *doc = NULL;
	fz_stream *stm = NULL;
	fz_var(doc);
	fz_var(stm);
	fz_try(ctx) {
		stm = fz_open_memory(ctx, data, len);
		doc = fz_open_document_with_stream(ctx, "application/pdf", stm);
		if (fz_needs_password(ctx, doc))
			fz_throw(ctx, FZ_ERROR_ARGUMENT, "password protected");
	}
	fz_always(ctx)
		fz_drop_stream(ctx, stm);
	fz_catch(ctx) {
		snprintf(err, errlen, "%s", fz_caught_message(ctx));
		fz_drop_document(ctx, doc);
		doc = NULL;
	}
	return doc;
}

void fl_drop_document(fz_context *ctx, fz_document *doc) { fz_drop_document(ctx, doc); }

int fl_count_pages(fz_context *ctx, fz_document *doc) {
	int n = 0;
	fz_try(ctx)
		n = fz_count_pages(ctx, doc);
	fz_catch(ctx)
		n = 0;
	return n;
}

/* A page's size in points, after its rotation; read from the page tree
 * without loading the page. */
int fl_page_size(fz_context *ctx, fz_document *doc, int i, float *w, float *h) {
	int ok = 1;
	fz_try(ctx) {
		pdf_document *pdf = pdf_specifics(ctx, doc);
		fz_rect r;
		if (pdf) {
			fz_matrix ctm;
			pdf_page_obj_transform(ctx, pdf_lookup_page_obj(ctx, pdf, i), &r, &ctm);
			r = fz_transform_rect(r, ctm);
		} else {
			fz_page *page = fz_load_page(ctx, doc, i);
			r = fz_bound_page(ctx, page);
			fz_drop_page(ctx, page);
		}
		*w = r.x1 - r.x0;
		*h = r.y1 - r.y0;
	}
	fz_catch(ctx)
		ok = 0;
	return ok;
}

/* Page `i` interpreted once into a display list (contents and
 * annotations), which can then be rendered any number of times, by any
 * thread. NULL on error. */
fz_display_list *fl_load_list(fz_context *ctx, fz_document *doc, int i) {
	fz_page *page = NULL;
	fz_display_list *list = NULL;
	fz_var(page);
	fz_var(list);
	fz_try(ctx) {
		page = fz_load_page(ctx, doc, i);
		list = fz_new_display_list_from_page(ctx, page);
	}
	fz_always(ctx)
		fz_drop_page(ctx, page);
	fz_catch(ctx)
		list = NULL;
	return list;
}

void fl_drop_list(fz_context *ctx, fz_display_list *list) { fz_drop_display_list(ctx, list); }

/* The page's size in points, as its display list has it. */
void fl_list_size(fz_context *ctx, fz_display_list *list, float *w, float *h) {
	fz_rect b = fz_bound_display_list(ctx, list);
	*w = b.x1 - b.x0;
	*h = b.y1 - b.y0;
}

/* Renders into `rgb` (packed RGB, w x h, stride w*3): the page scaled by
 * (sx, sy), with its top-left corner at (tx, ty) in the bitmap. White where
 * the page does not reach. 0 on error. */
int fl_render(fz_context *ctx, fz_display_list *list, unsigned char *rgb, int w, int h, float sx, float sy,
	float tx, float ty) {
	fz_pixmap *pix = NULL;
	fz_device *dev = NULL;
	int ok = 1;
	fz_var(pix);
	fz_var(dev);
	fz_try(ctx) {
		fz_rect b = fz_bound_display_list(ctx, list);
		fz_matrix ctm = fz_concat(fz_translate(-b.x0, -b.y0), fz_scale(sx, sy));
		ctm = fz_concat(ctm, fz_translate(tx, ty));
		pix = fz_new_pixmap_with_data(ctx, fz_device_rgb(ctx), w, h, NULL, 0, w * 3, rgb);
		fz_clear_pixmap_with_value(ctx, pix, 255);
		dev = fz_new_draw_device(ctx, fz_identity, pix);
		fz_run_display_list(ctx, list, dev, ctm, fz_make_rect(0, 0, w, h), NULL);
		fz_close_device(ctx, dev);
	}
	fz_always(ctx) {
		fz_drop_device(ctx, dev);
		fz_drop_pixmap(ctx, pix);
	}
	fz_catch(ctx) {
		memset(rgb, 255, (size_t)w * h * 3);
		ok = 0;
	}
	return ok;
}

/* Matches of `needle` (case-insensitive unless `exact`): up to `max` quads
 * as 8 floats each (ul, ur, ll, lr), in page points from its top-left, with
 * marks[i] set where a new match begins. Returns the number of quads. */
int fl_search(fz_context *ctx, fz_display_list *list, const char *needle, int exact, float *quads, int *marks,
	int max) {
	fz_stext_page *text = NULL;
	int n = 0;
	fz_var(text);
	fz_try(ctx) {
		fz_rect b = fz_bound_display_list(ctx, list);
		text = fz_new_stext_page_from_display_list(ctx, list, NULL);
		fz_quad *q = fz_malloc_array(ctx, max, fz_quad);
		fz_try(ctx) {
			n = fz_match_stext_page(ctx, text, needle, marks, q, max,
				exact ? FZ_SEARCH_EXACT : FZ_SEARCH_IGNORE_CASE);
			for (int i = 0; i < n; i++) {
				float *o = quads + 8 * i;
				o[0] = q[i].ul.x - b.x0; o[1] = q[i].ul.y - b.y0;
				o[2] = q[i].ur.x - b.x0; o[3] = q[i].ur.y - b.y0;
				o[4] = q[i].ll.x - b.x0; o[5] = q[i].ll.y - b.y0;
				o[6] = q[i].lr.x - b.x0; o[7] = q[i].lr.y - b.y0;
			}
		}
		fz_always(ctx)
			fz_free(ctx, q);
		fz_catch(ctx)
			fz_rethrow(ctx);
	}
	fz_always(ctx)
		fz_drop_stext_page(ctx, text);
	fz_catch(ctx)
		n = 0;
	return n;
}

typedef void (fl_outline_fn)(void *user, const char *title, int depth, int page, float x, float y);

static void walk(fz_context *ctx, fz_document *doc, fz_outline *o, int depth, fl_outline_fn *fn, void *user,
	int *left) {
	for (; o && *left > 0; o = o->next) {
		int page = -1;
		if (o->page.page >= 0)
			page = fz_page_number_from_location(ctx, doc, o->page);
		fn(user, o->title ? o->title : "", depth, page, o->x, o->y);
		(*left)--;
		if (o->down && depth < 32)
			walk(ctx, doc, o->down, depth + 1, fn, user, left);
	}
}

/* The outline, depth first, through `fn`: title, depth, page (-1 if it
 * goes nowhere) and where on the page, in points from its top-left. */
void fl_outline(fz_context *ctx, fz_document *doc, fl_outline_fn *fn, void *user) {
	fz_outline *o = NULL;
	fz_var(o);
	fz_try(ctx) {
		o = fz_load_outline(ctx, doc);
		int left = 20000;
		walk(ctx, doc, o, 0, fn, user, &left);
	}
	fz_always(ctx)
		fz_drop_outline(ctx, o);
	fz_catch(ctx) {
	}
}
